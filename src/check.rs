//! Check whether each profile's model server answers and lists its model.
//!
//! One `GET <base_url>/models` covers every profile that shares a server and
//! headers. Ollama, vLLM, llama.cpp, and most gateways serve this
//! OpenAI-compatible list, and reading it does not load a model.

use std::io::{ErrorKind, Read};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::header::HeaderMap;
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde_json::Value;

use crate::launch::Command;
use crate::profile::{Catalog, Profile};
use crate::provider;

/// How long a server has to answer, including the response body.
pub const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_LIST_BYTES: u64 = 4 << 20;
/// The most model names that a "model missing" message suggests.
const MAX_SUGGESTIONS: usize = 3;

/// What the launcher knows about a profile's server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The request has not finished.
    Checking,
    /// The server lists the profile's model.
    Ready,
    /// The server answered without a model list to compare.
    Reachable,
    /// The server lists other models, but not this one.
    ModelMissing,
    /// No answer, a server error, or refused credentials.
    Down,
    /// Not checked, for the reason in the message.
    Skipped,
}

impl State {
    /// How the menu and `list --check` name the state.
    pub fn label(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Ready => "ready",
            Self::Reachable => "reachable",
            Self::ModelMissing => "model missing",
            Self::Down => "down",
            Self::Skipped => "not checked",
        }
    }
}

/// A profile's server state, with the reason for anything but success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub state: State,
    pub message: Option<String>,
}

impl Status {
    fn new(state: State, message: Option<String>) -> Self {
        Self { state, message }
    }
}

/// What one server returned.
enum Reply {
    Models(Vec<String>),
    /// An answer without a readable model list, and why.
    Unlisted(String),
    /// No usable answer, and why.
    Down(String),
}

/// The latest server check for each profile in a menu or listing.
pub struct Checks {
    statuses: Vec<Option<Status>>,
    /// The server that each checked profile waits on, and the model to find.
    targets: Vec<Option<(usize, String)>>,
    /// Replies to the current round. Replacing it discards older rounds.
    replies: Option<Receiver<(usize, Reply)>>,
}

impl Checks {
    /// No checks yet: every status is `None` until `start`.
    pub fn new(profiles: usize) -> Self {
        Self {
            statuses: vec![None; profiles],
            targets: vec![None; profiles],
            replies: None,
        }
    }

    /// Whether checks have started, so that their results belong on screen.
    pub fn started(&self) -> bool {
        self.replies.is_some()
    }

    /// Whether any server has yet to answer.
    pub fn is_running(&self) -> bool {
        self.statuses
            .iter()
            .flatten()
            .any(|status| status.state == State::Checking)
    }

    pub fn status(&self, index: usize) -> Option<&Status> {
        self.statuses.get(index)?.as_ref()
    }

    /// Check every profile again in the background, with one request for each
    /// distinct server and set of headers. `commands` are the profiles' launch
    /// commands, and `args` the launch arguments that may change the model.
    pub fn start(&mut self, catalog: &Catalog, commands: &[Command], args: &[String]) {
        let (sender, receiver) = mpsc::channel();
        self.replies = Some(receiver);
        let client = provider::client().map_err(|error| format!("{error:#}"));
        let mut servers: Vec<(Url, HeaderMap)> = Vec::new();
        (self.targets, self.statuses) = catalog
            .profiles
            .iter()
            .zip(commands)
            .map(|(profile, command)| {
                let planned = match &client {
                    Ok(_) => request(profile, command, &catalog.codex_home, args),
                    Err(reason) => Err(reason.clone()),
                };
                match planned {
                    Ok((url, headers, model)) => {
                        let server = servers
                            .iter()
                            .position(|(known_url, known_headers)| {
                                *known_url == url && *known_headers == headers
                            })
                            .unwrap_or_else(|| {
                                servers.push((url, headers));
                                servers.len() - 1
                            });
                        (
                            Some((server, model)),
                            Some(Status::new(State::Checking, None)),
                        )
                    }
                    Err(reason) => (None, Some(Status::new(State::Skipped, Some(reason)))),
                }
            })
            .unzip();
        let Ok(client) = client else {
            return;
        };
        for (server, (url, headers)) in servers.into_iter().enumerate() {
            let sender = sender.clone();
            let spawned = std::thread::Builder::new()
                .name("server-check".to_owned())
                .spawn(move || {
                    // Sending fails once a newer round or an exit drops the receiver.
                    let _ = sender.send((server, fetch(client, url, headers)));
                });
            if let Err(error) = spawned {
                let reason = format!("cannot start the check: {error}");
                for (target, status) in self.targets.iter().zip(&mut self.statuses) {
                    if target
                        .as_ref()
                        .is_some_and(|(waiting, _)| *waiting == server)
                    {
                        *status = Some(Status::new(State::Skipped, Some(reason.clone())));
                    }
                }
            }
        }
    }

    /// Apply the replies that have arrived. Returns whether any status changed.
    pub fn poll(&mut self) -> bool {
        let Some(replies) = &self.replies else {
            return false;
        };
        let arrived: Vec<_> = replies.try_iter().collect();
        for (server, reply) in &arrived {
            self.apply(*server, reply);
        }
        !arrived.is_empty()
    }

    /// Wait for the servers that have yet to answer.
    pub fn wait(&mut self) {
        // Each request has its own timeout; this only guards against a hang.
        let deadline = Instant::now() + TIMEOUT + Duration::from_secs(1);
        while self.is_running() {
            let Some(replies) = &self.replies else {
                return;
            };
            match replies.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((server, reply)) => self.apply(server, &reply),
                Err(_) => return,
            }
        }
    }

    fn apply(&mut self, server: usize, reply: &Reply) {
        for (target, status) in self.targets.iter().zip(&mut self.statuses) {
            if let Some((waiting, model)) = target
                && *waiting == server
            {
                *status = Some(compare(reply, model));
            }
        }
    }
}

/// The model-list request for one profile, or why it is not checked.
fn request(
    profile: &Profile,
    command: &Command,
    codex_home: &Path,
    args: &[String],
) -> Result<(Url, HeaderMap, String), String> {
    // A wrapper may start the server or load credentials before Codex runs.
    if command.wrapper.is_some() {
        return Err(format!("launched through {}", command.program));
    }
    let (config, model) = provider::effective_config(profile, codex_home, args)
        .map_err(|error| format!("{error:#}"))?;
    let model = model.ok_or("no model is set")?;
    let connection = provider::connection(&config, |key| std::env::var(key).ok())?;
    let url = connection.url(&format!("{}/models", connection.base_path()));
    Ok((url, connection.headers, model))
}

/// Ask one server for its model list.
fn fetch(client: &Client, url: Url, headers: HeaderMap) -> Reply {
    let response = match client.get(url).headers(headers).timeout(TIMEOUT).send() {
        Ok(response) => response,
        Err(error) => return Reply::Down(describe(&error)),
    };
    let status = response.status();
    if !status.is_success() {
        return http_error(status);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_LIST_BYTES)
    {
        return Reply::Unlisted("the model list is too large".to_owned());
    }
    let mut bytes = Vec::new();
    if response
        .take(MAX_LIST_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Reply::Unlisted("the model list did not arrive".to_owned());
    }
    if bytes.len() as u64 > MAX_LIST_BYTES {
        return Reply::Unlisted("the model list is too large".to_owned());
    }
    match model_ids(&bytes) {
        Some(models) => Reply::Models(models),
        None => Reply::Unlisted("unreadable model list".to_owned()),
    }
}

/// A short reason for a failed request. The innermost cause omits the URL,
/// whose query parameters may hold credentials.
fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return format!("no answer within {} seconds", TIMEOUT.as_secs());
    }
    let mut innermost = None;
    let mut cause = std::error::Error::source(error);
    while let Some(error) = cause {
        innermost = Some(error);
        cause = error.source();
    }
    let Some(innermost) = innermost else {
        return "the request failed".to_owned();
    };
    match innermost
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind)
    {
        Some(ErrorKind::ConnectionRefused) => "connection refused".to_owned(),
        Some(ErrorKind::ConnectionReset) => "connection reset".to_owned(),
        Some(ErrorKind::HostUnreachable) => "host unreachable".to_owned(),
        Some(ErrorKind::NetworkUnreachable) => "network unreachable".to_owned(),
        Some(ErrorKind::TimedOut) => "connection timed out".to_owned(),
        _ => innermost.to_string(),
    }
}

/// Codex's requests would fail the same way as these errors, except for a
/// missing or redirected model list.
fn http_error(status: StatusCode) -> Reply {
    let code = status.as_u16();
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Reply::Down(format!("the server refused the credentials (HTTP {code})"))
        }
        _ if status.is_server_error() => Reply::Down(format!("server error (HTTP {code})")),
        _ => Reply::Unlisted(format!("no model list (HTTP {code})")),
    }
}

/// The model IDs in an OpenAI-compatible model list.
fn model_ids(bytes: &[u8]) -> Option<Vec<String>> {
    let list: Value = serde_json::from_slice(bytes).ok()?;
    list.get("data")?
        .as_array()?
        .iter()
        .map(|model| Some(model.get("id")?.as_str()?.to_owned()))
        .collect()
}

fn compare(reply: &Reply, model: &str) -> Status {
    match reply {
        Reply::Models(models) if lists(models, model) => Status::new(State::Ready, None),
        Reply::Models(models) => Status::new(State::ModelMissing, Some(suggest(models, model))),
        Reply::Unlisted(reason) => Status::new(State::Reachable, Some(reason.clone())),
        Reply::Down(reason) => Status::new(State::Down, Some(reason.clone())),
    }
}

/// Whether `models` includes `model`. Ollama adds `:latest` to an untagged name.
fn lists(models: &[String], model: &str) -> bool {
    models.iter().any(|id| {
        id == model || (!model.contains(':') && id.strip_suffix(":latest") == Some(model))
    })
}

/// What the server has instead: the same model with other tags, or the whole
/// list when it is short.
fn suggest(models: &[String], model: &str) -> String {
    let similar: Vec<&str> = models
        .iter()
        .map(String::as_str)
        .filter(|id| untagged(id) == untagged(model))
        .collect();
    if similar.is_empty() && models.len() > MAX_SUGGESTIONS {
        return format!("the server lists {} other models", models.len());
    }
    let shown = if similar.is_empty() {
        models.iter().map(String::as_str).collect()
    } else {
        similar
    };
    match shown.len() {
        0 => "the server lists no models".to_owned(),
        count if count <= MAX_SUGGESTIONS => format!("the server has {}", shown.join(", ")),
        count => format!(
            "the server has {}, and {} more",
            shown[..MAX_SUGGESTIONS].join(", "),
            count - MAX_SUGGESTIONS
        ),
    }
}

/// An Ollama model name without its `:tag`.
fn untagged(id: &str) -> &str {
    id.split_once(':').map_or(id, |(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(ids: &[&str]) -> Reply {
        Reply::Models(ids.iter().map(|id| (*id).to_owned()).collect())
    }

    fn message(reply: &Reply, model: &str) -> Option<String> {
        compare(reply, model).message
    }

    #[test]
    fn listed_models_match_exactly_or_with_ollamas_latest_tag() {
        let reply = models(&["gpt-oss:20b", "llama3:latest"]);
        assert_eq!(compare(&reply, "gpt-oss:20b").state, State::Ready);
        assert_eq!(compare(&reply, "llama3").state, State::Ready);
        assert_eq!(compare(&reply, "gpt-oss").state, State::ModelMissing);
        assert_eq!(compare(&reply, "llama3:8b").state, State::ModelMissing);
    }

    #[test]
    fn missing_models_suggest_other_tags_or_a_short_list() {
        let ollama = models(&[
            "glm-4.7-flash:q4_K_M",
            "gpt-oss:20b",
            "llama3.2:3b",
            "qwen3:30b",
            "qwen3-coder:30b",
        ]);
        assert_eq!(
            message(&ollama, "qwen3:30b-64k").as_deref(),
            Some("the server has qwen3:30b")
        );
        assert_eq!(
            message(&ollama, "other").as_deref(),
            Some("the server lists 5 other models")
        );
        assert_eq!(
            message(&models(&["served-name"]), "configured").as_deref(),
            Some("the server has served-name")
        );
        assert_eq!(
            message(&models(&[]), "m").as_deref(),
            Some("the server lists no models")
        );
        assert_eq!(
            message(&models(&["m:a", "m:b", "m:c", "m:d", "n:a"]), "m:e").as_deref(),
            Some("the server has m:a, m:b, m:c, and 1 more")
        );
    }

    #[test]
    fn model_lists_must_have_string_ids() {
        assert_eq!(
            model_ids(br#"{"object":"list","data":[{"id":"a"},{"id":"b"}]}"#),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(model_ids(br#"{"data":[{"id":7}]}"#), None);
        assert_eq!(model_ids(br#"{"models":[]}"#), None);
        assert_eq!(model_ids(b"not json"), None);
    }

    #[test]
    fn http_errors_separate_failures_from_missing_lists() {
        let down = |status| matches!(http_error(status), Reply::Down(_));
        assert!(down(StatusCode::UNAUTHORIZED));
        assert!(down(StatusCode::FORBIDDEN));
        assert!(down(StatusCode::BAD_GATEWAY));
        assert!(!down(StatusCode::NOT_FOUND));
        assert!(!down(StatusCode::FOUND));
    }

    #[test]
    fn refused_connections_are_reported_briefly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let url = Url::parse(&format!("http://{address}/v1/models?key=query-secret")).unwrap();
        let Reply::Down(reason) = fetch(provider::client().unwrap(), url, HeaderMap::new()) else {
            panic!("a closed port must be down");
        };
        assert_eq!(reason, "connection refused");
    }
}
