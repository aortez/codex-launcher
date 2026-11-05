//! Discover controls from the configured server's Ollama show API.

use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Url, redirect::Policy};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Key, Table, TableLike};

use crate::effort::{Choices, Control, query_arguments};
use crate::profile::{self, Profile};

const MAX_SHOW_BYTES: u64 = 4 << 20;
// Reuse the HTTP runtime. Dropping a per-query client can wait for an OS DNS
// lookup after the request itself times out, exceeding the control deadline.
static CLIENT: OnceLock<std::result::Result<Client, reqwest::Error>> = OnceLock::new();
// These are accepted by Codex's reasoning-effort configuration. Ollama can
// advertise arbitrary names, but those cannot all be passed through Codex.
const CODEX_LEVELS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

pub fn discover(
    profile: &Profile,
    home: &Path,
    args: &[String],
    deadline: Instant,
) -> Result<Option<Choices>> {
    let (config, model) = effective_config(profile, home, args)?;
    let Some((url, headers)) = connection(&config, |key| std::env::var(key).ok())? else {
        return Ok(None);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timed_out());
    }
    let client = CLIENT
        .get_or_init(|| Client::builder().redirect(Policy::none()).build())
        .as_ref()
        .map_err(|_| anyhow::anyhow!("could not initialize metadata HTTP client"))?;
    let request = client
        .post(url)
        .headers(headers)
        .json(&json!({"model": model}));
    // The whole response, including a slowly streamed body, shares the same
    // deadline as Codex discovery and every other key in the control request.
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timed_out());
    }
    let response = request.timeout(remaining).send()?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_SHOW_BYTES)
    {
        bail!("Ollama metadata is too large");
    }
    let mut bytes = Vec::new();
    let read = response.take(MAX_SHOW_BYTES + 1).read_to_end(&mut bytes);
    if Instant::now() >= deadline {
        return Err(timed_out());
    }
    read?;
    if bytes.len() as u64 > MAX_SHOW_BYTES {
        bail!("Ollama metadata is too large");
    }
    Ok(choices(&serde_json::from_slice(&bytes)?))
}

/// Resolve the same provider connection settings Codex uses. Injecting the
/// environment lookup lets tests cover defaults without changing process env.
fn connection(
    config: &DocumentMut,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<(Url, HeaderMap)>> {
    let provider_id = config
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or("openai");
    let provider = config
        .get("model_providers")
        .and_then(|providers| providers.get(provider_id))
        .and_then(Item::as_table_like);
    if provider_id == "ollama" {
        // Codex reserves built-in IDs; a custom table for one is invalid.
        if provider.is_some() {
            return Ok(None);
        }
        let base = env("CODEX_OSS_BASE_URL").unwrap_or_else(|| {
            let port = env("CODEX_OSS_PORT")
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(11434);
            format!("http://localhost:{port}/v1")
        });
        return Ok(show_url(&base).map(|url| (url, HeaderMap::new())));
    }
    let Some(provider) = provider else {
        return Ok(None);
    };
    // This request only uses the provider's own authentication settings. It
    // does not borrow Codex's OpenAI login or execute authentication wrappers.
    if provider.get("requires_openai_auth").and_then(Item::as_bool) == Some(true) {
        return Ok(None);
    }
    let Some(base) = provider.get("base_url").and_then(Item::as_str) else {
        return Ok(None);
    };
    let Some(mut url) = show_url(base) else {
        return Ok(None);
    };
    if let Some(params) = provider.get("query_params").and_then(Item::as_table_like) {
        for (key, value) in params.iter() {
            url.query_pairs_mut().append_pair(
                key,
                value.as_str().context("invalid provider query parameter")?,
            );
        }
    }
    let mut resolved_headers = HeaderMap::new();
    for (table, from_env) in [("http_headers", false), ("env_http_headers", true)] {
        if let Some(headers) = provider.get(table).and_then(Item::as_table_like) {
            for (key, value) in headers.iter() {
                let value = value.as_str().context("invalid provider header")?;
                let value = if from_env {
                    let Some(value) = env(value).filter(|value| !value.is_empty()) else {
                        continue;
                    };
                    value
                } else {
                    value.to_owned()
                };
                // HeaderMap replaces existing values case-insensitively;
                // repeated RequestBuilder::header calls would append them.
                resolved_headers.insert(
                    HeaderName::from_bytes(key.as_bytes())?,
                    HeaderValue::from_str(&value)?,
                );
            }
        }
    }
    let token = if let Some(key) = provider.get("env_key").and_then(Item::as_str) {
        // A configured env_key is required. Missing/empty values must not
        // silently fall back to an inline token that Codex would not use.
        let Some(token) = env(key).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        Some(token)
    } else {
        provider
            .get("experimental_bearer_token")
            .and_then(Item::as_str)
            .map(str::to_owned)
    };
    if let Some(token) = token {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))?;
        authorization.set_sensitive(true);
        resolved_headers.insert(AUTHORIZATION, authorization);
    }
    Ok(Some((url, resolved_headers)))
}

fn timed_out() -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "Ollama discovery timed out").into()
}

fn show_url(base: &str) -> Option<Url> {
    let mut url = Url::parse(base).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    // Preserve a reverse proxy's path prefix and origin; never follow a
    // redirect carrying provider headers to another server.
    let prefix = url.path().trim_end_matches('/').strip_suffix("/v1")?;
    let path = format!("{prefix}/api/show");
    url.set_path(&path);
    Some(url)
}

fn effective_config(
    profile: &Profile,
    home: &Path,
    args: &[String],
) -> Result<(DocumentMut, String)> {
    let mut config = profile::merged_config(home, profile)?;
    let (overrides, model) = query_arguments(args, config.get("model").and_then(Item::as_str))?;
    for pair in overrides.chunks_exact(2) {
        let (path, value) = pair[1].split_once('=').context("invalid config override")?;
        let keys = Key::parse(path.trim())?;
        let value = value
            .trim()
            .parse::<toml_edit::Value>()
            .unwrap_or_else(|_| toml_edit::Value::from(value.trim()));
        assign(config.as_table_mut(), &keys, Item::Value(value))?;
    }
    Ok((config, model))
}

fn assign(table: &mut dyn TableLike, keys: &[Key], value: Item) -> Result<()> {
    let (key, rest) = keys.split_first().context("empty config key")?;
    if rest.is_empty() {
        table.insert(key.get(), value);
    } else {
        let child = table.entry(key.get()).or_insert(Item::Table(Table::new()));
        assign(
            child
                .as_table_like_mut()
                .context("invalid config override table")?,
            rest,
            value,
        )?;
    }
    Ok(())
}

fn choices(show: &Value) -> Option<Choices> {
    let advertised = show.get("thinking").filter(|value| !value.is_null());
    let (levels, control, source) = if let Some(thinking) = advertised {
        let values = thinking.get("values")?.as_array()?;
        if values.is_empty() {
            return None;
        }
        let mut levels = Vec::new();
        let mut named = false;
        for value in values {
            let level = match value {
                Value::Bool(false) => "none",
                Value::Bool(true) => "medium",
                Value::String(level) => {
                    named = true;
                    level.as_str()
                }
                _ => return None,
            };
            if CODEX_LEVELS.contains(&level) && !levels.iter().any(|v| v == level) {
                levels.push(level.to_owned());
            }
        }
        if values.iter().all(|v| v == false) {
            (levels, Control::OffOnly, "Ollama /api/show")
        } else if levels.is_empty() {
            // Metadata exists but Codex cannot express these controls.
            (levels, Control::Unsupported, "Ollama /api/show")
        } else {
            (
                levels,
                if named {
                    Control::Levels
                } else {
                    Control::Toggle
                },
                "Ollama /api/show",
            )
        }
    } else {
        let capabilities = show.get("capabilities")?.as_array()?;
        if capabilities.iter().any(|value| !value.is_string()) {
            return None;
        }
        if !capabilities.iter().any(|v| v == "completion") {
            return None;
        }
        if !capabilities.iter().any(|v| v == "thinking") {
            (vec!["none".into()], Control::OffOnly, "Ollama /api/show")
        } else {
            // Older servers (including 0.32.3) report capabilities but no
            // thinking.values. Only infer controls for verified families.
            // https://github.com/ollama/ollama/blob/v0.32.3/openai/responses.go
            // https://github.com/ollama/ollama/blob/v0.32.3/server/routes.go
            // https://github.com/ollama/ollama/blob/v0.32.3/model/renderers/qwen35.go
            match show.get("details")?.get("family")?.as_str()? {
                "gptoss" | "gpt-oss" => (
                    vec!["low".into(), "medium".into(), "high".into()],
                    Control::Levels,
                    "Ollama model family",
                ),
                "qwen35moe" | "glm4moelite" => (
                    vec!["none".into(), "medium".into()],
                    Control::Toggle,
                    "Ollama model family",
                ),
                _ => return None,
            }
        }
    };
    Some(Choices {
        levels,
        control,
        metadata_source: Some(source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ollama_defaults_and_overrides_match_codex() {
        let config: DocumentMut = "model_provider='ollama'".parse().unwrap();
        for (base, port, expected) in [
            (None, None, "http://localhost:11434/api/show"),
            (None, Some("12345"), "http://localhost:12345/api/show"),
            (
                Some("http://localhost:54321/proxy/v1"),
                Some("12345"),
                "http://localhost:54321/proxy/api/show",
            ),
        ] {
            let (url, headers) = connection(&config, |key| match key {
                "CODEX_OSS_BASE_URL" => base.map(str::to_owned),
                "CODEX_OSS_PORT" => port.map(str::to_owned),
                _ => None,
            })
            .unwrap()
            .unwrap();
            assert_eq!(url.as_str(), expected);
            assert!(headers.is_empty());
        }
        let invalid: DocumentMut =
            "model_provider='ollama'\n[model_providers.ollama]\nbase_url='http://wrong/v1'"
                .parse()
                .unwrap();
        assert!(connection(&invalid, |_| None).unwrap().is_none());
        let custom: DocumentMut =
            "model_provider='lab'\n[model_providers.lab]\nbase_url='http://custom/v1'"
                .parse()
                .unwrap();
        let (url, _) = connection(&custom, |_| Some("http://wrong/v1".into()))
            .unwrap()
            .unwrap();
        assert_eq!(url.as_str(), "http://custom/api/show");
    }

    #[test]
    fn advertised_controls_take_precedence_over_family_and_capabilities() {
        for (values, levels, control) in [
            (
                json!(["high", "low", "high"]),
                vec!["high", "low"],
                Control::Levels,
            ),
            (
                json!([false, true]),
                vec!["none", "medium"],
                Control::Toggle,
            ),
            (json!([true]), vec!["medium"], Control::Toggle),
            (json!([false]), vec!["none"], Control::OffOnly),
            (
                json!([false, "low", "high"]),
                vec!["none", "low", "high"],
                Control::Levels,
            ),
            (json!(["custom-budget"]), vec![], Control::Unsupported),
        ] {
            let result = choices(&json!({"thinking":{"values":values}, "details":{"family":"gptoss"}, "capabilities":["completion","thinking"]})).unwrap();
            assert_eq!(result.levels, levels);
            assert_eq!(result.control, control);
            assert_eq!(result.metadata_source, Some("Ollama /api/show"));
        }
        for values in [json!([]), json!([7]), json!(null)] {
            assert!(choices(&json!({"thinking":{"values":values}, "details":{"family":"gptoss"}, "capabilities":["completion","thinking"]})).is_none());
        }
    }

    #[test]
    fn older_servers_use_capabilities_and_verified_families_not_model_names() {
        for family in ["gptoss", "gpt-oss", "qwen35moe", "glm4moelite"] {
            let result = choices(
                &json!({"details":{"family":family}, "capabilities":["completion","thinking"]}),
            )
            .unwrap();
            assert_eq!(result.metadata_source, Some("Ollama model family"));
            if family.starts_with("gpt") {
                assert_eq!(result.levels, ["low", "medium", "high"]);
            } else {
                assert_eq!(result.levels, ["none", "medium"]);
                assert_eq!(result.control, Control::Toggle);
            }
        }
        let nonthinking = choices(
            &json!({"details":{"family":"qwen3next"}, "capabilities":["completion","tools"]}),
        )
        .unwrap();
        assert_eq!(nonthinking.control, Control::OffOnly);
        assert_eq!(nonthinking.levels, ["none"]);
        for data in [
            json!({}),
            json!({"model":"gpt-oss"}),
            json!({"capabilities":["thinking"]}),
            json!({"capabilities":["completion","thinking"],"details":{"family":"unknown"}}),
        ] {
            assert!(choices(&data).is_none());
        }
    }

    #[test]
    fn metadata_url_keeps_the_configured_origin_and_proxy_prefix() {
        assert_eq!(
            show_url("http://localhost:11434/v1/").unwrap().as_str(),
            "http://localhost:11434/api/show"
        );
        assert_eq!(
            show_url("https://example.com/ollama/v1").unwrap().as_str(),
            "https://example.com/ollama/api/show"
        );
        for url in [
            "https://example.com/other",
            "file:///v1",
            "https://user:pass@example.com/v1",
            "https://example.com/v1?token=secret",
        ] {
            assert!(show_url(url).is_none());
        }
    }

    #[test]
    fn discovery_resolves_merged_profiles_and_runtime_overrides() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "model='base'\nmodel_provider='local'\n[model_providers.local]\nbase_url='http://base/v1'\nhttp_headers={ X-Test='base' }\n").unwrap();
        std::fs::write(
            home.path().join("work.config.toml"),
            "model='profile'\n[model_providers.local]\nbase_url='http://profile/v1'\n",
        )
        .unwrap();
        let catalog = profile::load(home.path()).unwrap();
        let profile = catalog.find("work").unwrap();
        let args = [
            "-m=runtime",
            "-c",
            "model='config-override'",
            "-c",
            "model_providers.local.base_url='http://runtime/v1'",
            "--",
            "-m=literal",
        ]
        .map(str::to_owned);
        let (config, model) = effective_config(profile, home.path(), &args).unwrap();
        assert_eq!(model, "runtime");
        assert_eq!(config["model"].as_str(), Some("runtime"));
        assert_eq!(
            config["model_providers"]["local"]["base_url"].as_str(),
            Some("http://runtime/v1")
        );
        assert_eq!(
            config["model_providers"]["local"]["http_headers"]["X-Test"].as_str(),
            Some("base")
        );
        assert!(effective_config(profile, home.path(), &["--oss".into()]).is_err());
    }
}
