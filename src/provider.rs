//! How Codex reaches a profile's model provider: the merged configuration,
//! and the URL, query parameters, and headers of its requests.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Url, redirect::Policy};
use toml_edit::{DocumentMut, Item, Key, Table, TableLike};

use crate::effort::query_arguments;
use crate::profile::{self, BUILTIN_PROVIDERS, Profile};

// Reuse the HTTP runtime. Dropping a per-query client can wait for an OS DNS
// lookup after the request itself times out, exceeding the control deadline.
static CLIENT: OnceLock<std::result::Result<Client, reqwest::Error>> = OnceLock::new();

/// The shared HTTP client. It never follows a redirect, which would carry the
/// provider's headers to another server. It keeps no idle connection either,
/// so that a status command that counts a server's clients does not count the
/// launcher.
pub fn client() -> Result<&'static Client> {
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .redirect(Policy::none())
                .pool_max_idle_per_host(0)
                .build()
        })
        .as_ref()
        .map_err(|_| anyhow::anyhow!("could not initialize the HTTP client"))
}

/// Where a provider's requests go, and the headers they carry.
#[derive(Debug)]
pub struct Connection {
    /// The base URL, which has no query string of its own.
    base: Url,
    query: Vec<(String, String)>,
    pub headers: HeaderMap,
}

impl Connection {
    /// The base URL's path without a trailing slash, such as `/proxy/v1`.
    pub fn base_path(&self) -> &str {
        self.base.path().trim_end_matches('/')
    }

    /// The URL of `path` on the provider's server, with its query parameters.
    pub fn url(&self, path: &str) -> Url {
        let mut url = self.base.clone();
        url.set_path(path);
        if !self.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&self.query);
        }
        url
    }
}

/// Resolve the provider connection that Codex uses, or say why the launcher
/// cannot use it. Injecting the environment lookup lets tests cover defaults
/// without changing the process environment.
pub fn connection(
    config: &DocumentMut,
    env: impl Fn(&str) -> Option<String>,
) -> std::result::Result<Connection, String> {
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
            return Err("the built-in ollama provider cannot be redefined".to_owned());
        }
        let base = env("CODEX_OSS_BASE_URL").unwrap_or_else(|| {
            let port = env("CODEX_OSS_PORT")
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(11434);
            format!("http://localhost:{port}/v1")
        });
        return Ok(Connection {
            base: base_url(&base)?,
            query: Vec::new(),
            headers: HeaderMap::new(),
        });
    }
    let Some(provider) = provider else {
        return Err(if BUILTIN_PROVIDERS.contains(&provider_id) {
            format!("built-in {provider_id} provider")
        } else {
            format!("provider {provider_id} is not defined")
        });
    };
    // This request only uses the provider's own authentication settings. It
    // does not borrow Codex's OpenAI login or execute authentication wrappers.
    if provider.get("requires_openai_auth").and_then(Item::as_bool) == Some(true) {
        return Err("uses the OpenAI login".to_owned());
    }
    let base = provider
        .get("base_url")
        .and_then(Item::as_str)
        .ok_or("the provider has no base_url")?;
    let base = base_url(base)?;
    let mut query = Vec::new();
    if let Some(params) = provider.get("query_params").and_then(Item::as_table_like) {
        for (key, value) in params.iter() {
            let value = value.as_str().ok_or("invalid provider query parameter")?;
            query.push((key.to_owned(), value.to_owned()));
        }
    }
    let mut resolved_headers = HeaderMap::new();
    for (table, from_env) in [("http_headers", false), ("env_http_headers", true)] {
        if let Some(headers) = provider.get(table).and_then(Item::as_table_like) {
            for (key, value) in headers.iter() {
                let invalid = || format!("invalid provider header {key}");
                let value = value.as_str().ok_or_else(invalid)?;
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
                    HeaderName::from_bytes(key.as_bytes()).map_err(|_| invalid())?,
                    HeaderValue::from_str(&value).map_err(|_| invalid())?,
                );
            }
        }
    }
    let token = if let Some(key) = provider.get("env_key").and_then(Item::as_str) {
        // A configured env_key is required. Missing/empty values must not
        // silently fall back to an inline token that Codex would not use.
        let Some(token) = env(key).filter(|value| !value.is_empty()) else {
            return Err(format!("{key} is not set"));
        };
        Some(token)
    } else {
        provider
            .get("experimental_bearer_token")
            .and_then(Item::as_str)
            .map(str::to_owned)
    };
    if let Some(token) = token {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "invalid bearer token".to_owned())?;
        authorization.set_sensitive(true);
        resolved_headers.insert(AUTHORIZATION, authorization);
    }
    Ok(Connection {
        base,
        query,
        headers: resolved_headers,
    })
}

/// A base URL that requests can extend. Query values belong in
/// `query_params`, and credentials or a fragment would not survive the
/// request paths built on it.
fn base_url(text: &str) -> std::result::Result<Url, String> {
    let url = Url::parse(text).map_err(|_| "base_url is not a URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("base_url is not an http or https URL".to_owned());
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("base_url has credentials, a query, or a fragment".to_owned());
    }
    Ok(url)
}

/// The merged base and profile configuration with runtime `-c` overrides
/// applied, and the model that the launch would use, if any is set.
pub fn effective_config(
    profile: &Profile,
    home: &Path,
    args: &[String],
) -> Result<(DocumentMut, Option<String>)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    #[test]
    fn builtin_ollama_defaults_and_overrides_match_codex() {
        let builtin = config("model_provider='ollama'");
        for (base, port, expected) in [
            (None, None, "http://localhost:11434/v1"),
            (None, Some("12345"), "http://localhost:12345/v1"),
            (
                Some("http://localhost:54321/proxy/v1"),
                Some("12345"),
                "http://localhost:54321/proxy/v1",
            ),
        ] {
            let connection = connection(&builtin, |key| match key {
                "CODEX_OSS_BASE_URL" => base.map(str::to_owned),
                "CODEX_OSS_PORT" => port.map(str::to_owned),
                _ => None,
            })
            .unwrap();
            assert_eq!(connection.base.as_str(), expected);
            assert!(connection.headers.is_empty());
        }
        let invalid =
            config("model_provider='ollama'\n[model_providers.ollama]\nbase_url='http://wrong/v1'");
        assert!(connection(&invalid, |_| None).is_err());
        let custom =
            config("model_provider='lab'\n[model_providers.lab]\nbase_url='http://custom/v1'");
        let connection = connection(&custom, |_| Some("http://wrong/v1".into())).unwrap();
        assert_eq!(connection.base.as_str(), "http://custom/v1");
    }

    #[test]
    fn urls_keep_the_base_path_and_add_query_parameters() {
        let lab = config(
            "model_provider='lab'\n[model_providers.lab]\nbase_url='https://example.com/proxy/v1/'\n\
             query_params={ route='a b', token='query-secret' }\n",
        );
        let connection = connection(&lab, |_| None).unwrap();
        assert_eq!(connection.base_path(), "/proxy/v1");
        assert_eq!(
            connection
                .url(&format!("{}/models", connection.base_path()))
                .as_str(),
            "https://example.com/proxy/v1/models?route=a+b&token=query-secret"
        );
    }

    #[test]
    fn unusable_providers_explain_why() {
        let reason = |text: &str| connection(&config(text), |_| None).unwrap_err();
        assert_eq!(reason(""), "built-in openai provider");
        assert_eq!(
            reason("model_provider='lmstudio'"),
            "built-in lmstudio provider"
        );
        assert_eq!(
            reason("model_provider='nowhere'"),
            "provider nowhere is not defined"
        );
        let lab = "model_provider='lab'\n[model_providers.lab]\n";
        assert_eq!(
            reason(&format!(
                "{lab}base_url='http://h/v1'\nrequires_openai_auth=true"
            )),
            "uses the OpenAI login"
        );
        assert_eq!(reason(lab), "the provider has no base_url");
        assert_eq!(
            reason(&format!("{lab}base_url='http://h/v1'\nenv_key='LAB_KEY'")),
            "LAB_KEY is not set"
        );
        for base in [
            "https://user:pass@example.com/v1",
            "https://example.com/v1?token=secret",
            "https://example.com/v1#part",
        ] {
            assert_eq!(
                reason(&format!("{lab}base_url={base:?}")),
                "base_url has credentials, a query, or a fragment"
            );
        }
        assert_eq!(
            reason(&format!("{lab}base_url='file:///v1'")),
            "base_url is not an http or https URL"
        );
    }

    #[test]
    fn effective_config_merges_profiles_and_runtime_overrides() {
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
        assert_eq!(model.as_deref(), Some("runtime"));
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
