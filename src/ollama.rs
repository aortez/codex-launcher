//! Discover controls from the configured server's Ollama show API.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, bail};
use reqwest::Url;
use serde_json::{Value, json};

use crate::effort::{Choices, Control};
use crate::profile::Profile;
use crate::provider::{self, Connection};

const MAX_SHOW_BYTES: u64 = 4 << 20;
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
    let (config, model) = provider::effective_config(profile, home, args)?;
    let Some(model) = model else {
        return Ok(None);
    };
    let Ok(connection) = provider::connection(&config, |key| std::env::var(key).ok()) else {
        return Ok(None);
    };
    let Some(url) = show_url(&connection) else {
        return Ok(None);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timed_out());
    }
    let client = provider::client()?;
    let request = client
        .post(url)
        .headers(connection.headers)
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

fn timed_out() -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "Ollama discovery timed out").into()
}

/// The Ollama show endpoint beside an OpenAI-compatible `/v1` base URL. It
/// keeps a reverse proxy's path prefix and origin.
fn show_url(connection: &Connection) -> Option<Url> {
    let prefix = connection.base_path().strip_suffix("/v1")?;
    Some(connection.url(&format!("{prefix}/api/show")))
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
        let show = |base: &str| {
            let config: toml_edit::DocumentMut =
                format!("model_provider='lab'\n[model_providers.lab]\nbase_url={base:?}\n")
                    .parse()
                    .unwrap();
            show_url(&provider::connection(&config, |_| None).unwrap()).map(String::from)
        };
        assert_eq!(
            show("http://localhost:11434/v1/").as_deref(),
            Some("http://localhost:11434/api/show")
        );
        assert_eq!(
            show("https://example.com/ollama/v1").as_deref(),
            Some("https://example.com/ollama/api/show")
        );
        assert_eq!(show("https://example.com/other"), None);
    }
}
