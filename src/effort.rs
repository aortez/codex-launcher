//! Effort discovery through Codex, with local metadata as a fallback.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::profile::{self, Profile};

const COMMON_LEVELS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
const MAX_CATALOG_BYTES: u64 = 16 << 20;
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize)]
pub struct Choices {
    pub levels: Vec<String>,
    /// None means these are common values, not verified model capabilities.
    pub metadata_source: Option<&'static str>,
}

impl Choices {
    /// Query on demand. Return false on timeout so a later action can retry.
    pub fn discover(
        &mut self,
        profile: &Profile,
        codex_home: &Path,
        args: &[String],
        codex: &Path,
        timeout: Duration,
    ) -> bool {
        match query_catalog(profile, codex_home, args, codex, timeout) {
            Ok((catalog, model)) => {
                if let Some(levels) = catalog_levels(&catalog, &model) {
                    self.levels = levels;
                    self.metadata_source = Some("codex debug models");
                }
                true
            }
            Err(error) => error
                .downcast_ref::<std::io::Error>()
                .is_none_or(|error| error.kind() != std::io::ErrorKind::TimedOut),
        }
    }

    pub fn load(profile: &Profile, codex_home: &Path, args: &[String]) -> Self {
        // Command-line model/provider overrides can select a different model;
        // do not trust profile metadata in that case.
        let changed_model = args.iter().enumerate().any(|(i, arg)| {
            arg == "--model"
                || arg.starts_with("--model=")
                || arg.starts_with("-m")
                || arg == "--oss"
                || arg.starts_with("--local-provider")
                || arg.starts_with("--profile")
                || arg.starts_with("-p")
                || config_argument(args, i).is_some_and(|(_, value)| {
                    value.split_once('=').is_some_and(|(key, _)| {
                        matches!(
                            key.trim(),
                            "model" | "model_provider" | "model_catalog_json" | "model_catalog_url"
                        )
                    })
                })
        });
        if !changed_model && let Some(model) = profile.model_name() {
            let catalog = if let Some(setting) = &profile.model_catalog {
                Some((codex_home.join(&setting.value), "model catalog"))
            } else if profile.provider.id == "openai" && profile.provider.base_url.is_none() {
                Some((codex_home.join("models_cache.json"), "Codex cache"))
            } else {
                None
            };
            if let Some((path, source)) = catalog
                && let Some(levels) = read_levels(&path, model)
            {
                return Self {
                    levels,
                    metadata_source: Some(source),
                };
            }
        }
        let mut levels: Vec<_> = COMMON_LEVELS.iter().map(|s| (*s).to_owned()).collect();
        if let Some(setting) = &profile.effort
            && valid_level(&setting.value)
            && !levels.contains(&setting.value)
        {
            levels.push(setting.value.clone());
        }
        Self {
            levels,
            metadata_source: None,
        }
    }
}

fn valid_level(level: &str) -> bool {
    !level.is_empty()
        && level.len() <= 40
        && level
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn read_levels(path: &Path, model: &str) -> Option<Vec<String>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return None;
    }
    let catalog: Value = serde_json::from_slice(&bytes).ok()?;
    catalog_levels(&catalog, model)
}

fn catalog_levels(catalog: &Value, model: &str) -> Option<Vec<String>> {
    let entry = catalog
        .get("models")?
        .as_array()?
        .iter()
        .find(|m| m["slug"] == model)?;
    let mut levels = Vec::new();
    for level in entry.get("supported_reasoning_levels")?.as_array()? {
        let effort = level.get("effort")?.as_str()?;
        if !valid_level(effort) {
            return None;
        }
        if !levels.iter().any(|s| s == effort) {
            levels.push(effort.to_owned());
        }
    }
    // An empty advertised list is meaningful: only inheritance is available.
    Some(levels)
}

fn query_catalog(
    profile: &Profile,
    codex_home: &Path,
    args: &[String],
    codex: &Path,
    timeout: Duration,
) -> Result<(Value, String)> {
    let deadline = Instant::now() + timeout;
    let config = profile::merged_config(codex_home, profile)?;
    let (config_args, model) = query_arguments(args, config.get("model").and_then(|m| m.as_str()))?;
    // The debug command does not accept --profile. Give it the merged config
    // through a private snapshot, so credentials never become process arguments.
    let snapshot = if profile.is_default() {
        None
    } else {
        Some(profile_snapshot(codex_home, &config.to_string())?)
    };
    let query_home = snapshot.as_ref().map_or(codex_home, |dir| dir.path());
    let mut output = tempfile::tempfile()?;
    if Instant::now() >= deadline {
        return Err(query_timeout());
    }
    let mut child = Command::new(codex)
        .args(["debug", "models"])
        .args(config_args)
        .env("CODEX_HOME", query_home)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let process_group = rustix::process::Pid::from_child(&child);
    let result = (|| {
        loop {
            if output.metadata()?.len() > MAX_CATALOG_BYTES {
                bail!("model catalog is too large");
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!("codex debug models failed");
                }
                output.seek(SeekFrom::Start(0))?;
                let mut bytes = Vec::new();
                (&mut output)
                    .take(MAX_CATALOG_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_CATALOG_BYTES {
                    bail!("model catalog is too large");
                }
                return Ok((serde_json::from_slice(&bytes)?, model));
            }
            if Instant::now() >= deadline {
                return Err(query_timeout());
            }
            std::thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    // Wrappers may leave descendants behind even after they exit. Terminate
    // the isolated group on every path, then reap the immediate child.
    let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
    let _ = child.wait();
    result
}

fn query_timeout() -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "codex debug models timed out").into()
}

fn profile_snapshot(codex_home: &Path, config: &str) -> Result<tempfile::TempDir> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

    let home = codex_home.canonicalize()?;
    // A sibling keeps ../ paths relative to the same parent. Symlinks preserve
    // access to auth and profile-local files without copying their contents.
    let snapshot = tempfile::Builder::new()
        .prefix(".codex-launcher-models-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(home.parent().context("Codex home has no parent")?)?;
    for entry in fs::read_dir(&home)? {
        let entry = entry?;
        if entry.file_name() != "config.toml" {
            symlink(entry.path(), snapshot.path().join(entry.file_name()))?;
        }
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(snapshot.path().join("config.toml"))?;
    file.write_all(config.as_bytes())?;
    Ok(snapshot)
}

/// Forward config overrides, never the runtime subcommand or prompt. Model
/// flags are runtime-only too, but their equivalent config key works here.
fn query_arguments(args: &[String], initial_model: Option<&str>) -> Result<(Vec<String>, String)> {
    let mut model = initial_model.map(str::to_owned);
    let mut model_flag_value = None;
    let mut forwarded = Vec::new();
    let mut index = 0;
    while index < args.len() && args[index] != "--" {
        let arg = &args[index];
        if let Some((count, assignment)) = config_argument(args, index) {
            if let Some((key, value)) = assignment.split_once('=')
                && key.trim() == "model"
            {
                model = Some(config_string(value));
            }
            forwarded.extend(["-c".to_owned(), assignment.to_owned()]);
            index += count;
            continue;
        }
        let model_flag = if arg == "-m" || arg == "--model" {
            index += 1;
            Some(args.get(index).context("missing model argument")?.as_str())
        } else {
            arg.strip_prefix("--model=").or_else(|| {
                arg.strip_prefix("-m")
                    .map(|s| s.strip_prefix('=').unwrap_or(s))
                    .filter(|s| !s.is_empty())
            })
        };
        if let Some(value) = model_flag {
            model_flag_value = Some(value.to_owned());
        } else if arg.starts_with("--profile")
            || arg.starts_with("-p")
            || arg == "--oss"
            || arg.starts_with("--local-provider")
        {
            bail!("runtime provider/profile selection cannot be queried");
        }
        index += 1;
    }
    // The runtime's explicit --model takes precedence over config overrides.
    if let Some(value) = model_flag_value {
        forwarded.extend([
            "-c".to_owned(),
            format!("model={}", toml_edit::Value::from(value.as_str())),
        ]);
        model = Some(value);
    }
    Ok((forwarded, model.context("no configured model to match")?))
}

fn config_string(value: &str) -> String {
    value
        .trim()
        .parse::<toml_edit::Value>()
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| value.trim().to_owned())
}

/// Recognize Codex's config flag forms, returning the span and assignment.
pub fn config_argument(args: &[String], index: usize) -> Option<(usize, &str)> {
    let arg = args.get(index)?;
    if arg == "-c" || arg == "--config" {
        return Some((2, args.get(index + 1)?));
    }
    arg.strip_prefix("--config=")
        .or_else(|| {
            arg.strip_prefix("-c")
                .map(|s| s.strip_prefix('=').unwrap_or(s))
        })
        .filter(|s| !s.is_empty())
        .map(|value| (1, value))
}

pub fn is_effort_assignment(assignment: &str) -> bool {
    assignment
        .split_once('=')
        .is_some_and(|(key, _)| key.trim() == "model_reasoning_effort")
}

/// The last explicit effort before `--`, following Codex's override order.
pub fn argument_effort(args: &[String]) -> Option<String> {
    let mut effort = None;
    let mut index = 0;
    while index < args.len() && args[index] != "--" {
        if let Some((count, assignment)) = config_argument(args, index) {
            if is_effort_assignment(assignment) {
                let value = assignment.split_once('=')?.1.trim();
                effort = Some(config_string(value));
            }
            index += count;
        } else {
            index += 1;
        }
    }
    effort
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fake_codex(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("codex");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn query_forwards_config_and_model_but_not_prompts() {
        let args = [
            "resume",
            "--last",
            "--model=explicit",
            "-c",
            "model='configured'",
            "--config=model_provider='gateway'",
            "a prompt",
            "--",
            "-m",
            "literal",
        ]
        .map(str::to_owned);
        let (forwarded, model) = query_arguments(&args, Some("base")).unwrap();
        assert_eq!(model, "explicit");
        assert_eq!(
            forwarded,
            [
                "-c",
                "model='configured'",
                "-c",
                "model_provider='gateway'",
                "-c",
                "model=\"explicit\""
            ]
        );
    }

    #[test]
    fn failed_queries_keep_the_local_fallback_and_timeout_reaps_the_child() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = 'test-model'\n").unwrap();
        fs::write(home.path().join("models_cache.json"), r#"{"models":[{"slug":"test-model","supported_reasoning_levels":[{"effort":"high"}]}]}"#).unwrap();
        let catalog = profile::load(home.path()).unwrap();
        let profile = catalog.find("default").unwrap();
        for body in [
            "exit 1",
            "printf 'invalid json'",
            "printf '{\"models\":[]}'",
        ] {
            let codex = fake_codex(home.path(), body);
            let mut choices = Choices::load(profile, home.path(), &[]);
            assert!(choices.discover(profile, home.path(), &[], &codex, QUERY_TIMEOUT));
            assert_eq!(choices.metadata_source, Some("Codex cache"));
            assert_eq!(choices.levels, ["high"]);
        }
        let codex = fake_codex(
            home.path(),
            "printf '%s' \"$$\" > \"$CODEX_HOME/query.pid\"\nwhile :; do :; done",
        );
        let started = Instant::now();
        let result = query_catalog(
            profile,
            home.path(),
            &[],
            &codex,
            Duration::from_millis(100),
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = fs::read_to_string(home.path().join("query.pid")).unwrap();
        assert!(!Path::new("/proc").join(pid).exists());
    }

    #[test]
    fn discovery_terminates_wrapper_descendants_on_timeout_and_exit() {
        use rustix::process::{Pid, Signal, kill_process};

        // Kill the known descendant even if an assertion fails, so a
        // regression does not leave the test's sleeper running.
        struct Descendant(Pid);
        impl Drop for Descendant {
            fn drop(&mut self) {
                let _ = kill_process(self.0, Signal::KILL);
            }
        }

        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = 'test-model'\n").unwrap();
        let catalog = profile::load(home.path()).unwrap();
        let profile = catalog.find("default").unwrap();
        for ending in ["wait", "printf '{\"models\":[]}'"] {
            let codex = fake_codex(
                home.path(),
                &format!(
                    "/bin/sleep 60 &\nprintf '%s' \"$!\" > \"$CODEX_HOME/descendant.pid\"\n{ending}"
                ),
            );
            let result = query_catalog(
                profile,
                home.path(),
                &[],
                &codex,
                Duration::from_millis(200),
            );
            let pid: i32 = fs::read_to_string(home.path().join("descendant.pid"))
                .unwrap()
                .parse()
                .unwrap();
            let descendant = Descendant(Pid::from_raw(pid).unwrap());
            if ending == "wait" {
                assert!(result.unwrap_err().to_string().contains("timed out"));
            } else {
                assert!(result.is_ok());
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let status = fs::read_to_string(format!("/proc/{pid}/status"));
                // The orphan may briefly remain a zombie until its reaper runs.
                let running = status.is_ok_and(|s| {
                    !s.lines()
                        .any(|line| line.starts_with("State:") && line.contains("Z (zombie)"))
                });
                if !running {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "discovery descendant {pid} survived cleanup"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(descendant);
        }
    }

    #[test]
    fn snapshot_merges_profile_tables_and_preserves_private_file_access() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = 'base'\n[model_providers.gateway]\nname = 'Gateway'\nbase_url = 'https://base.example'\n").unwrap();
        fs::write(home.path().join("gateway.config.toml"), "model = 'profile-model'\nmodel_provider = 'gateway'\n[model_providers.gateway]\nbase_url = 'https://profile.example'\nexperimental_bearer_token = 'test-secret'\n").unwrap();
        fs::write(home.path().join("auth.json"), "{\"test\":true}").unwrap();
        let catalog = profile::load(home.path()).unwrap();
        let merged = profile::merged_config(home.path(), catalog.find("gateway").unwrap()).unwrap();
        let snapshot = profile_snapshot(home.path(), &merged.to_string()).unwrap();
        let path = snapshot.path().to_owned();
        assert_eq!(path.parent(), home.path().parent());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(path.join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let config = fs::read_to_string(path.join("config.toml"))
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(config["model"].as_str(), Some("profile-model"));
        assert_eq!(
            config["model_providers"]["gateway"]["name"].as_str(),
            Some("Gateway")
        );
        assert_eq!(
            config["model_providers"]["gateway"]["base_url"].as_str(),
            Some("https://profile.example")
        );
        assert_eq!(
            config["model_providers"]["gateway"]["experimental_bearer_token"].as_str(),
            Some("test-secret")
        );
        assert_eq!(
            fs::read_to_string(path.join("auth.json")).unwrap(),
            "{\"test\":true}"
        );
        drop(snapshot);
        assert!(!path.exists());
        assert!(home.path().join("auth.json").exists());
    }

    #[test]
    fn catalogs_are_scoped_to_the_model_and_provider() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = 'test-model'\n").unwrap();
        fs::write(
            home.path().join("gateway.config.toml"),
            "model_provider = 'gateway'\n",
        )
        .unwrap();
        let metadata = r#"{"models":[{"slug":"test-model","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]}]}"#;
        fs::write(home.path().join("models_cache.json"), metadata).unwrap();
        let catalog = profile::load(home.path()).unwrap();
        let base = catalog.find("default").unwrap();
        let choices = Choices::load(base, home.path(), &[]);
        assert_eq!(choices.levels, ["low", "high"]);
        assert_eq!(choices.metadata_source, Some("Codex cache"));
        assert_eq!(
            Choices::load(catalog.find("gateway").unwrap(), home.path(), &[]).metadata_source,
            None
        );
        assert_eq!(
            Choices::load(base, home.path(), &["--model=other".into()]).metadata_source,
            None
        );
        fs::write(
            home.path().join("gateway.config.toml"),
            "model_provider = 'gateway'\nmodel_catalog_json = 'gateway.json'\n",
        )
        .unwrap();
        fs::write(home.path().join("gateway.json"), metadata).unwrap();
        let catalog = profile::load(home.path()).unwrap();
        assert_eq!(
            Choices::load(catalog.find("gateway").unwrap(), home.path(), &[]).metadata_source,
            Some("model catalog")
        );
    }

    #[test]
    fn empty_and_unreadable_metadata_are_distinct() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "model = 'test-model'\n").unwrap();
        let catalog = profile::load(home.path()).unwrap();
        let profile = catalog.find("default").unwrap();
        let path = home.path().join("models_cache.json");
        fs::write(
            &path,
            r#"{"models":[{"slug":"test-model","supported_reasoning_levels":[]}]}"#,
        )
        .unwrap();
        let choices = Choices::load(profile, home.path(), &[]);
        assert!(choices.levels.is_empty());
        assert!(choices.metadata_source.is_some());
        fs::write(&path, "invalid JSON").unwrap();
        let choices = Choices::load(profile, home.path(), &[]);
        assert!(choices.levels.contains(&"high".to_owned()));
        assert!(choices.metadata_source.is_none());
    }
}
