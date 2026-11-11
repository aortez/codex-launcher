//! End-to-end tests: run the real binary headless and drive it with `ctl`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_codex-launcher");

fn fixture_home() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-home")
}

fn writable_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(fixture_home()).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), home.path().join(entry.file_name())).unwrap();
    }
    home
}

#[test]
fn codex_catalog_takes_precedence_and_is_queried_once_per_profile() {
    use std::os::unix::fs::PermissionsExt;
    let home = writable_home();
    let bin = tempfile::tempdir().unwrap();
    let codex = bin.path().join("codex");
    std::fs::write(&codex, "#!/bin/sh\n[ \"$1\" = debug ] && [ \"$2\" = models ] || exit 1\nprintf 'query\\n' >> \"$0.calls\"\nprintf '%s\\n' \"$@\" >> \"$0.args\"\nprintf '%s\\n' \"$CODEX_HOME\" >> \"$0.homes\"\n/bin/cat \"$CODEX_HOME/live-catalog.json\"\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.path().join("models_cache.json"),
        r#"{"models":[{"slug":"gpt-base","supported_reasoning_levels":[{"effort":"low"}]}]}"#,
    )
    .unwrap();
    std::fs::write(home.path().join("live-catalog.json"), r#"{"models":[{"slug":"gpt-base","supported_reasoning_levels":[{"effort":"high"},{"effort":"ultra"}]},{"slug":"gpt-team","supported_reasoning_levels":[{"effort":"medium"},{"effort":"xhigh"}]}]}"#).unwrap();
    let launcher = Headless::start_with_path(home.path(), &["--", "resume", "--last"], bin.path());
    assert!(!bin.path().join("codex.calls").exists());
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(
        state["effort_choices"]["metadata_source"],
        "codex debug models"
    );
    assert_eq!(
        state["effort_choices"]["levels"],
        serde_json::json!(["high", "ultra"])
    );
    assert!(!launcher.ctl(&["effort", "low"]).status.success());
    launcher.ctl_json(&["key", "esc", "e", "esc"]);
    launcher.ctl_json(&["effort", "ultra"]);
    launcher.ctl_json(&["save-effort"]);
    launcher.ctl_json(&["key", "e", "esc"]);
    assert_eq!(
        std::fs::read_to_string(bin.path().join("codex.calls")).unwrap(),
        "query\n"
    );
    launcher.ctl_json(&["select", "gateway"]);
    let state = launcher.ctl_json(&["effort", "xhigh"]);
    assert_eq!(
        state["effort_choices"]["metadata_source"],
        "codex debug models"
    );
    assert_eq!(
        state["effort_choices"]["levels"],
        serde_json::json!(["medium", "xhigh"])
    );
    assert_eq!(
        std::fs::read_to_string(bin.path().join("codex.args")).unwrap(),
        "debug\nmodels\ndebug\nmodels\n"
    );
    let homes = std::fs::read_to_string(bin.path().join("codex.homes")).unwrap();
    assert_eq!(homes.lines().next().unwrap(), home.path().to_str().unwrap());
    let snapshot = Path::new(homes.lines().nth(1).unwrap());
    assert_ne!(snapshot, home.path());
    assert!(!snapshot.exists(), "temporary config was not cleaned up");
    launcher.ctl_json(&["quit"]);
}

#[test]
fn batched_discovery_shares_a_deadline_and_deferred_profiles_can_retry() {
    use std::os::unix::fs::PermissionsExt;
    let home = writable_home();
    let bin = tempfile::tempdir().unwrap();
    let codex = bin.path().join("codex");
    std::fs::write(&codex, "#!/bin/sh\n/bin/sleep 60 &\nwait\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launcher = Headless::start_with_path(home.path(), &[], bin.path());
    let started = Instant::now();
    let state = launcher.ctl_json(&["key", "e", "esc", "down", "e", "esc", "down", "e", "esc"]);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "discovery exceeded the request budget"
    );
    assert_eq!(state["selected"], "local");
    assert_eq!(state["status"], "selecting");
    assert!(state["effort_picker"].is_null());
    assert_eq!(launcher.ctl_json(&["state"])["selected"], "local");

    // The last lookup was skipped, not cached as a permanent failure.
    std::fs::write(&codex, "#!/bin/sh\nprintf '%s' '{\"models\":[{\"slug\":\"qwen3-coder:30b\",\"supported_reasoning_levels\":[{\"effort\":\"high\"}]}]}'\n").unwrap();
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(
        state["effort_choices"]["metadata_source"],
        "codex debug models"
    );
    assert_eq!(
        state["effort_choices"]["levels"],
        serde_json::json!(["high"])
    );
    launcher.ctl_json(&["quit"]);
}

#[test]
fn short_model_equals_flag_matches_discovered_model() {
    use std::os::unix::fs::PermissionsExt;
    let home = writable_home();
    let bin = tempfile::tempdir().unwrap();
    let codex = bin.path().join("codex");
    std::fs::write(&codex, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s' '{\"models\":[{\"slug\":\"review-model\",\"supported_reasoning_levels\":[{\"effort\":\"medium\"}]}]}'\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launcher = Headless::start_with_path(home.path(), &["--", "-m=review-model"], bin.path());
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(
        state["effort_choices"]["metadata_source"],
        "codex debug models"
    );
    assert_eq!(
        state["effort_choices"]["levels"],
        serde_json::json!(["medium"])
    );
    assert_eq!(
        std::fs::read_to_string(bin.path().join("codex.args")).unwrap(),
        "debug\nmodels\n-c\nmodel=\"review-model\"\n"
    );
    launcher.ctl_json(&["quit"]);
}

#[test]
fn effort_picker_applies_cancels_resets_and_launches() {
    let home = writable_home();
    let base_path = home.path().join("config.toml");
    let original = std::fs::read_to_string(&base_path).unwrap();
    std::fs::write(home.path().join("models_cache.json"), r#"{"models":[{"slug":"gpt-base","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"ultra"}]}]}"#).unwrap();
    let launcher = Headless::start_in(home.path(), &["--", "resume", "--last"]);
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(
        state["effort_choices"]["levels"],
        serde_json::json!(["low", "high", "ultra"])
    );
    let screen = launcher.ctl(&["screen"]);
    let screen = String::from_utf8(screen.stdout).unwrap();
    assert!(screen.contains("Use profile setting (high)"), "{screen}");
    assert!(screen.contains("s: Save to config.toml"), "{screen}");
    let state = launcher.ctl_json(&["key", "down", "enter"]);
    assert_eq!(state["status"], "selecting");
    assert_eq!(state["profile"]["effort"]["source"], "override");
    assert_eq!(state["menu"][0]["effort"], "low");
    assert!(state["effort_picker"].is_null());
    let screen = launcher.ctl(&["screen"]);
    assert!(
        String::from_utf8(screen.stdout)
            .unwrap()
            .contains("effort    low  (override)")
    );
    assert_eq!(
        launcher.ctl_json(&["key", "e", "end", "esc"])["effort_override"],
        "low"
    );
    assert!(!launcher.ctl(&["effort", "medium"]).status.success());
    assert_eq!(launcher.ctl_json(&["state"])["effort_override"], "low");
    launcher.ctl_json(&["select", "gateway"]);
    let state = launcher.ctl_json(&["effort", "medium"]);
    assert!(state["effort_choices"]["metadata_source"].is_null());
    assert_eq!(
        launcher.ctl_json(&["select", "default"])["effort_override"],
        "low"
    );
    let state = launcher.ctl_json(&["key", "e", "home", "enter"]);
    assert!(state["effort_override"].is_null());
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "resume", "--last"])
    );
    assert_eq!(state["profile"]["effort"]["value"], "high");
    launcher.ctl_json(&["effort", "ultra"]);
    launcher.ctl_json(&["launch"]);
    assert_eq!(
        launcher.finish(),
        "codex -c 'model_reasoning_effort=\"ultra\"' resume --last\n"
    );
    assert_eq!(std::fs::read_to_string(base_path).unwrap(), original);
}

#[test]
fn effort_saves_refresh_inherited_values_and_report_failures() {
    let home = writable_home();
    let launcher = Headless::start_in(home.path(), &[]);
    assert!(!launcher.ctl(&["save-effort"]).status.success());
    launcher.ctl_json(&["effort", "low"]);
    let state = launcher.ctl_json(&["save-effort"]);
    assert!(state["effort_override"].is_null());
    assert_eq!(state["profile"]["effort"]["source"], "base");
    assert_eq!(state["menu"][1]["effort"], "low");
    assert_eq!(state["menu"][2]["effort"], "medium");
    launcher.ctl_json(&["select", "gateway"]);
    // Save the highlighted level directly from the picker.
    let state = launcher.ctl_json(&["key", "e", "home", "down", "s"]);
    assert_eq!(state["profile"]["effort"]["value"], "none");
    assert_eq!(state["profile"]["effort"]["source"], "profile");
    assert!(state["effort_override"].is_null());
    let path = home.path().join("gateway.config.toml");
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("sk-test-secret"));
    assert!(saved.contains("model_reasoning_effort = \"none\""));
    let screen = launcher.ctl(&["screen"]);
    assert!(
        !String::from_utf8(screen.stdout)
            .unwrap()
            .contains("sk-test-secret")
    );
    launcher.ctl_json(&["effort", "high"]);
    std::fs::write(&path, "invalid TOML [").unwrap();
    let output = launcher.ctl(&["save-effort"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot save invalid TOML"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "invalid TOML [");
    assert_eq!(launcher.ctl_json(&["state"])["effort_override"], "high");
    launcher.ctl_json(&["quit"]);
    assert_eq!(launcher.finish(), "");
}

#[test]
fn effort_override_wins_over_arguments_and_reset_restores_them() {
    let home = writable_home();
    let launcher = Headless::start_in(
        home.path(),
        &["--", "resume", "--last", "-c", "model_reasoning_effort=low"],
    );
    assert_eq!(
        launcher.ctl_json(&["state"])["profile"]["effort"]["source"],
        "arguments"
    );
    let state = launcher.ctl_json(&["effort", "high"]);
    assert_eq!(
        state["command"],
        serde_json::json!([
            "codex",
            "-c",
            "model_reasoning_effort=\"high\"",
            "resume",
            "--last"
        ])
    );
    // Saving must not silently revert this launch to the command-line value.
    let state = launcher.ctl_json(&["save-effort"]);
    assert_eq!(state["effort_override"], "high");
    let state = launcher.ctl_json(&["effort", "profile"]);
    assert_eq!(state["profile"]["effort"]["value"], "low");
    assert_eq!(
        state["command"],
        serde_json::json!([
            "codex",
            "resume",
            "--last",
            "-c",
            "model_reasoning_effort=low"
        ])
    );
    launcher.ctl_json(&["quit"]);
}

/// A headless launcher with its own socket directory, killed on drop.
struct Headless {
    child: Child,
    runtime: tempfile::TempDir,
}

impl Headless {
    fn start(args: &[&str]) -> Self {
        Self::start_in(&fixture_home(), args)
    }

    fn start_in(codex_home: &Path, args: &[&str]) -> Self {
        Self::start_with_path(codex_home, args, Path::new("/nonexistent"))
    }

    fn start_with_path(codex_home: &Path, args: &[&str], path: &Path) -> Self {
        Self::start_with_env(codex_home, args, path, &[])
    }

    /// Start with server checks off, so fixture servers see only the requests
    /// that a test makes. Press `r` to check, or use `launch` to check at once.
    fn start_with_env(
        codex_home: &Path,
        args: &[&str],
        path: &Path,
        env: &[(&str, Option<&str>)],
    ) -> Self {
        let args: Vec<&str> = std::iter::once("--no-check")
            .chain(args.iter().copied())
            .collect();
        Self::launch(codex_home, &args, path, env)
    }

    fn launch(codex_home: &Path, args: &[&str], path: &Path, env: &[(&str, Option<&str>)]) -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let mut command = Command::new(BIN);
        command
            .arg("--headless")
            .args(args)
            .env("CODEX_HOME", codex_home)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("PATH", path)
            // Existing fixtures include built-in Ollama. Never query a real
            // local server unless this test supplies its own loopback URL.
            .env_remove("CODEX_OSS_BASE_URL")
            .env("CODEX_OSS_PORT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        let child = command.spawn().unwrap();
        let socket = runtime
            .path()
            .join("codex-launcher")
            .join(format!("{}.sock", child.id()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "the launcher never listened");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self { child, runtime }
    }

    fn ctl(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("ctl")
            .args(args)
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .output()
            .unwrap()
    }

    fn ctl_json(&self, args: &[&str]) -> Value {
        let output = self.ctl(args);
        assert!(
            output.status.success(),
            "ctl {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The state once no server check is still running.
    fn checked_state(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = self.ctl_json(&["state"]);
            let menu = state["menu"].as_array().unwrap();
            if !menu.iter().any(|entry| entry["server"] == "checking") {
                return state;
            }
            assert!(Instant::now() < deadline, "server checks never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for the launcher to exit and return its stdout.
    fn finish(mut self) -> String {
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the launcher exited with {status}");
        let mut stdout = String::new();
        self.child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        stdout
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn ctl_reads_and_drives_the_menu() {
    let launcher = Headless::start(&["--size", "90x24", "--", "resume", "--last"]);

    let state = launcher.ctl_json(&["state"]);
    assert_eq!(state["status"], "selecting");
    assert_eq!(state["mode"], "headless");
    assert_eq!(state["selected"], "default");
    assert!(state["menu"][0].get("server").is_none());
    assert!(state["profile"].get("server").is_none());
    let names: Vec<_> = state["menu"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["default", "gateway", "local"]);
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "resume", "--last"])
    );

    let state = launcher.ctl_json(&["key", "down"]);
    assert_eq!(state["selected"], "gateway");
    assert_eq!(
        state["command_line"],
        "codex --profile gateway resume --last"
    );

    let state = launcher.ctl_json(&["select", "local"]);
    assert_eq!(state["selected"], "local");
    assert_eq!(state["profile"]["model"]["value"], "qwen3-coder:30b");
    assert_eq!(
        state["profile"]["auto_compact_token_limit"]["value"],
        "57344"
    );

    let screen = launcher.ctl(&["screen"]);
    let screen = String::from_utf8(screen.stdout).unwrap();
    assert_eq!(screen.lines().count(), 24, "{screen}");
    assert!(screen.contains("> local"), "{screen}");
    assert!(
        screen.contains("Local Ollama model on this machine."),
        "{screen}"
    );

    let instances = launcher.ctl_json(&["instances"]);
    assert_eq!(instances[0]["pid"], launcher.child.id());
    assert_eq!(instances[0]["selected"], "local");

    let state = launcher.ctl_json(&["launch"]);
    assert_eq!(state["status"], "launching");
    let stdout = launcher.finish();
    assert_eq!(stdout, "codex --profile local resume --last\n");
}

#[test]
fn bad_requests_change_nothing() {
    let launcher = Headless::start(&[]);
    let output = launcher.ctl(&["key", "down", "sideways"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown key"));

    let output = launcher.ctl(&["select", "missing"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("default, gateway, local"));

    assert_eq!(launcher.ctl_json(&["state"])["selected"], "default");

    let state = launcher.ctl_json(&["quit"]);
    assert_eq!(state["status"], "quitting");
    assert_eq!(launcher.finish(), "");
}

#[test]
fn help_takes_ctl_keys_until_it_closes() {
    let launcher = Headless::start(&["--size", "80x12"]);
    assert!(launcher.ctl_json(&["state"])["help"].is_null());
    let state = launcher.ctl_json(&["key", "?", "end"]);
    let max = state["help"]["max_scroll"].as_u64().unwrap();
    assert!(max > 0, "{state}");
    assert_eq!(state["help"]["scroll"], max);
    let screen = String::from_utf8(launcher.ctl(&["screen"]).stdout).unwrap();
    assert!(screen.contains("Quit."), "{screen}");

    // While the help is open, q closes it instead of quitting.
    let state = launcher.ctl_json(&["key", "q"]);
    assert!(state["help"].is_null(), "{state}");
    assert_eq!(state["status"], "selecting");
    launcher.ctl_json(&["quit"]);
}

#[test]
fn batched_keys_scroll_like_separate_keys() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("a.config.toml"), "model = \"short\"\n").unwrap();
    let long: String = std::iter::once("model = \"long\"\n".to_owned())
        .chain((1..=60).map(|line| format!("# filler {line}.\n")))
        .collect();
    std::fs::write(home.path().join("b.config.toml"), long).unwrap();

    let scroll_after = |requests: &[&[&str]]| {
        let launcher = Headless::start_in(home.path(), &["--size", "80x20"]);
        launcher.ctl_json(&["select", "a"]);
        let mut state = Value::Null;
        for request in requests {
            let mut args = vec!["key"];
            args.extend_from_slice(request);
            state = launcher.ctl_json(&args);
        }
        assert_eq!(state["selected"], "b");
        state["details"]["scroll"].as_u64().unwrap()
    };

    let separate = scroll_after(&[&["down"], &["pgdn"]]);
    assert!(separate > 0, "PgDn did not scroll");
    assert_eq!(scroll_after(&[&["down", "pgdn"]]), separate);
}

#[test]
fn ctl_without_a_launcher_fails_clearly() {
    let runtime = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .args(["ctl", "state"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no running codex-launcher"));
}

#[test]
fn direct_launch_dry_run_and_listing() {
    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .env("CODEX_HOME", fixture_home())
            .env("PATH", "/nonexistent")
            .output()
            .unwrap()
    };

    let output = run(&["gateway", "--dry-run", "exec", "it's"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "codex --profile gateway exec 'it'\\''s'\n"
    );

    let output = run(&["list"]);
    let listing = String::from_utf8(output.stdout).unwrap();
    assert!(listing.starts_with("PROFILE"), "{listing}");
    assert!(listing.contains("local    qwen3-coder:30b"), "{listing}");

    let output = run(&["show", "gateway", "--json"]);
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown["provider"]["name"], "Team gateway");
    let file = shown["file"].as_str().unwrap();
    assert!(file.contains("GATEWAY_API_KEY"), "{file}");
    assert!(!file.contains("sk-test-secret"), "{file}");

    let output = run(&["nope"]);
    assert!(!output.status.success());
}

/// A local HTTP fixture, with bounded waits and cleanup even on failed assertions.
struct OllamaServer {
    base_url: String,
    requests: std::sync::mpsc::Receiver<(String, Value)>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl OllamaServer {
    fn start() -> Self {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}/proxy/v1", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut slow_seen = false;
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    headers.push_str(&line);
                }
                if headers.starts_with("GET ") {
                    tx.send((headers, Value::Null)).unwrap();
                    let response = serde_json::json!({"object": "list", "data": [
                        {"id": "toggle"}, {"id": "coder"}, {"id": "qwen3:30b"},
                    ]})
                    .to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    );
                    continue;
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let model = request["model"].as_str().unwrap().to_owned();
                tx.send((headers, request)).unwrap();
                if model == "redirect" {
                    write!(
                        stream,
                        "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                if model == "oversized" {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                if model == "slow" && !slow_seen {
                    slow_seen = true;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 9999\r\n\r\n").unwrap();
                    while !stopping.load(Ordering::Relaxed) {
                        if stream.write_all(b" ").is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    continue;
                }
                let response = match model.as_str() {
                    "toggle" => serde_json::json!({"thinking":{"values":[false,true]}}),
                    "off" => serde_json::json!({"thinking":{"values":[false]}}),
                    "coder" | "qwen3-coder:30b" => serde_json::json!({"capabilities":["completion","tools"],"details":{"family":"qwen3next"}}),
                    "unknown" => serde_json::json!({"capabilities":["completion","thinking"],"details":{"family":"unverified"}}),
                    _ => serde_json::json!({"capabilities":["completion","thinking"],"details":{"family":"gpt-oss"}}),
                }.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            }
        });
        Self {
            base_url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn home(&self, model: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), format!("model={model:?}\nmodel_provider='lab'\nmodel_reasoning_effort='high'\n[model_providers.lab]\nname='Local server'\nbase_url={:?}\nhttp_headers={{ X-Test='header-secret' }}\n", self.base_url)).unwrap();
        home
    }
}

impl Drop for OllamaServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn ollama_controls_drive_the_picker_commands_and_profile_saves() {
    let server = OllamaServer::start();
    let home = server.home("custom-oss-alias");
    for (name, model) in [("toggle", "toggle"), ("coder", "coder")] {
        std::fs::write(
            home.path().join(format!("{name}.config.toml")),
            format!("model={model:?}\n"),
        )
        .unwrap();
    }
    // No Codex executable: server metadata still works.
    let launcher = Headless::start_in(home.path(), &[]);
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(
        state["effort_choices"]["levels"],
        serde_json::json!(["low", "medium", "high"])
    );
    assert_eq!(state["effort_choices"]["control"], "levels");
    assert!(!launcher.ctl(&["effort", "ultra"]).status.success());
    launcher.ctl_json(&["key", "esc", "e", "esc"]);
    assert_eq!(
        server.requests.try_iter().count(),
        1,
        "discovery should be cached"
    );
    launcher.ctl_json(&["select", "toggle"]);
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(state["effort_choices"]["control"], "toggle");
    let screen = String::from_utf8(launcher.ctl(&["screen"]).stdout).unwrap();
    assert!(
        screen.contains("Thinking off (none)") && screen.contains("Thinking on (medium)"),
        "{screen}"
    );
    let state = launcher.ctl_json(&["effort", "none"]);
    assert!(
        state["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "model_reasoning_effort=\"none\"")
    );
    launcher.ctl_json(&["save-effort"]);
    assert!(
        std::fs::read_to_string(home.path().join("toggle.config.toml"))
            .unwrap()
            .contains("model_reasoning_effort = \"none\"")
    );
    launcher.ctl_json(&["select", "coder"]);
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(state["effort_choices"]["control"], "off_only");
    assert_eq!(
        state["effort_choices"]["levels"],
        serde_json::json!(["none"])
    );
    let screen = String::from_utf8(launcher.ctl(&["screen"]).stdout).unwrap();
    assert!(screen.contains("does not support thinking"), "{screen}");
    assert!(screen.contains("Thinking off (none)"), "{screen}");
    assert!(!launcher.ctl(&["effort", "high"]).status.success());
    launcher.ctl_json(&["effort", "profile"]);
    for (headers, _) in server.requests.try_iter() {
        assert!(
            headers.starts_with("POST /proxy/api/show HTTP/1.1"),
            "{headers}"
        );
        assert!(headers.contains("x-test: header-secret"), "{headers}");
    }
    launcher.ctl_json(&["quit"]);
}

#[test]
fn ollama_slow_body_shares_batch_deadline_and_can_retry() {
    let server = OllamaServer::start();
    let home = server.home("slow");
    for name in ["a", "b"] {
        std::fs::write(
            home.path().join(format!("{name}.config.toml")),
            "model='toggle'\n",
        )
        .unwrap();
    }
    let launcher = Headless::start_in(home.path(), &[]);
    let started = Instant::now();
    let state = launcher.ctl_json(&["key", "e", "esc", "down", "e", "esc", "down", "e", "esc"]);
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(state["selected"], "b");
    launcher.ctl_json(&["select", "default"]);
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(
        state["effort_choices"]["metadata_source"],
        "Ollama model family"
    );
    launcher.ctl_json(&["quit"]);
}

#[test]
fn ollama_unverified_redirected_and_oversized_responses_remain_unknown() {
    let server = OllamaServer::start();
    for model in ["unknown", "redirect", "oversized"] {
        let home = server.home(model);
        let launcher = Headless::start_in(home.path(), &[]);
        let state = launcher.ctl_json(&["key", "e"]);
        assert!(state["effort_choices"]["metadata_source"].is_null());
        assert_eq!(state["effort_choices"]["control"], "unknown");
        launcher.ctl_json(&["quit"]);
        assert_eq!(server.requests.try_iter().count(), 1);
    }
}

#[test]
fn ollama_fallback_uses_runtime_target_after_codex_has_no_matching_model() {
    use std::os::unix::fs::PermissionsExt;
    let server = OllamaServer::start();
    let home = server.home("wrong-model");
    let bin = tempfile::tempdir().unwrap();
    let codex = bin.path().join("codex");
    std::fs::write(&codex, "#!/bin/sh\nprintf '{\"models\":[]}'\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = format!(
        "model_providers.runtime={{name='Runtime',base_url={:?}}}",
        server.base_url
    );
    let launcher = Headless::start_with_path(
        home.path(),
        &[
            "--",
            "-m=toggle",
            "-c",
            "model_provider='runtime'",
            "-c",
            &config,
        ],
        bin.path(),
    );
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(state["effort_choices"]["control"], "toggle");
    let (headers, body) = server
        .requests
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(body["model"], "toggle");
    assert!(
        !headers.contains("header-secret"),
        "headers belong to the overridden provider"
    );
    launcher.ctl_json(&["quit"]);
}

#[test]
fn nonthinking_models_can_disable_inherited_effort_and_save_or_reset() {
    let server = OllamaServer::start();
    for model in ["off", "coder"] {
        let home = server.home("toggle");
        std::fs::write(
            home.path().join("work.config.toml"),
            format!("model={model:?}\n"),
        )
        .unwrap();
        let launcher = Headless::start_in(home.path(), &[]);
        let state = launcher.ctl_json(&["select", "work"]);
        assert_eq!(state["profile"]["effort"]["value"], "high");
        assert_eq!(state["profile"]["effort"]["source"], "base");
        let state = launcher.ctl_json(&["key", "e", "end", "enter"]);
        assert_eq!(state["effort_override"], "none");
        assert!(
            state["command"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "model_reasoning_effort=\"none\"")
        );
        let state = launcher.ctl_json(&["effort", "profile"]);
        assert_eq!(state["profile"]["effort"]["value"], "high");
        assert!(state["effort_override"].is_null());
        launcher.ctl_json(&["effort", "none"]);
        assert!(!launcher.ctl(&["effort", "medium"]).status.success());
        assert_eq!(launcher.ctl_json(&["state"])["effort_override"], "none");
        let state = launcher.ctl_json(&["save-effort"]);
        assert_eq!(state["profile"]["effort"]["value"], "none");
        assert_eq!(state["profile"]["effort"]["source"], "profile");
        assert!(
            std::fs::read_to_string(home.path().join("work.config.toml"))
                .unwrap()
                .contains("model_reasoning_effort = \"none\"")
        );
        assert!(
            std::fs::read_to_string(home.path().join("config.toml"))
                .unwrap()
                .contains("model_reasoning_effort='high'")
        );
        launcher.ctl_json(&["quit"]);
    }
}

fn header_values<'a>(headers: &'a str, name: &str) -> Vec<&'a str> {
    headers
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
        .collect()
}

#[test]
fn ollama_headers_and_bearer_tokens_follow_codex_precedence() {
    let server = OllamaServer::start();
    for (env_key, inline_token, env_value, expected) in [
        (true, true, Some("env-token"), Some("Bearer env-token")),
        (true, true, None, None),
        (true, true, Some(""), None),
        (false, true, None, Some("Bearer inline-token")),
        (false, false, None, Some("Bearer header-token")),
    ] {
        let home = server.home("toggle");
        let config = home.path().join("config.toml");
        let mut text = std::fs::read_to_string(&config).unwrap().replace("http_headers={ X-Test='header-secret' }", "http_headers={ X-Test='static', X-Keep='keep-static', X-Empty='keep-empty', Authorization='Bearer static-token' }");
        text.push_str("env_http_headers={ x-test='EFFORT_TEST_HEADER', X-Keep='EFFORT_TEST_ABSENT', X-Empty='EFFORT_TEST_EMPTY', X-Missing='EFFORT_TEST_ABSENT', authorization='EFFORT_TEST_AUTH' }\n");
        if env_key {
            text.push_str("env_key='EFFORT_TEST_TOKEN'\n");
        }
        if inline_token {
            text.push_str("experimental_bearer_token='inline-token'\n");
        }
        std::fs::write(config, text).unwrap();
        let launcher = Headless::start_with_env(
            home.path(),
            &[],
            Path::new("/nonexistent"),
            &[
                ("EFFORT_TEST_HEADER", Some("env-header")),
                ("EFFORT_TEST_ABSENT", None),
                ("EFFORT_TEST_EMPTY", Some("")),
                ("EFFORT_TEST_AUTH", Some("Bearer header-token")),
                ("EFFORT_TEST_TOKEN", env_value),
            ],
        );
        let state = launcher.ctl_json(&["key", "e"]);
        if let Some(expected) = expected {
            assert_eq!(state["effort_choices"]["control"], "toggle");
            let (headers, _) = server
                .requests
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            assert_eq!(header_values(&headers, "X-Test"), ["env-header"]);
            assert_eq!(header_values(&headers, "X-Keep"), ["keep-static"]);
            assert_eq!(header_values(&headers, "X-Empty"), ["keep-empty"]);
            assert!(header_values(&headers, "X-Missing").is_empty());
            assert_eq!(header_values(&headers, "Authorization"), [expected]);
        } else {
            assert_eq!(state["effort_choices"]["control"], "unknown");
            assert!(
                server.requests.try_recv().is_err(),
                "must not use the inline token when env_key is missing/empty"
            );
        }
        launcher.ctl_json(&["quit"]);
    }
}

#[test]
fn ollama_query_parameters_keep_profile_and_runtime_overrides() {
    let server = OllamaServer::start();
    let home = server.home("toggle");
    let config = home.path().join("config.toml");
    let mut base = std::fs::read_to_string(&config).unwrap();
    base.push_str(
        "query_params={ route='base', token='query-secret', api-version='2026-01-01' }\n",
    );
    std::fs::write(&config, base).unwrap();
    std::fs::write(
        home.path().join("work.config.toml"),
        "[model_providers.lab]\nquery_params={ route='profile' }\n",
    )
    .unwrap();
    let launcher = Headless::start_in(
        home.path(),
        &[
            "--",
            "-c",
            "model_providers.lab.query_params.route='runtime / qa&x=1+#%'",
        ],
    );
    launcher.ctl_json(&["select", "work"]);
    let state = launcher.ctl_json(&["key", "e"]);
    assert_eq!(state["effort_choices"]["control"], "toggle");
    let (headers, _) = server
        .requests
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let path = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let url = reqwest::Url::parse(&format!("http://fixture{path}")).unwrap();
    assert_eq!(url.path(), "/proxy/api/show");
    let params: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
    assert_eq!(params.len(), 3);
    assert_eq!(params["route"], "runtime / qa&x=1+#%");
    assert_eq!(params["token"], "query-secret");
    assert_eq!(params["api-version"], "2026-01-01");
    launcher.ctl_json(&["quit"]);
}

#[test]
fn builtin_ollama_fixture_uses_base_url_or_port_without_a_provider_table() {
    let server = OllamaServer::start();
    let port = reqwest::Url::parse(&server.base_url)
        .unwrap()
        .port()
        .unwrap()
        .to_string();
    for (base, port, expected_path) in [
        (Some(server.base_url.as_str()), "1", "/proxy/api/show"),
        (None, port.as_str(), "/api/show"),
    ] {
        let home = writable_home();
        let launcher = Headless::start_with_env(
            home.path(),
            &[],
            Path::new("/nonexistent"),
            &[("CODEX_OSS_BASE_URL", base), ("CODEX_OSS_PORT", Some(port))],
        );
        launcher.ctl_json(&["select", "local"]);
        let state = launcher.ctl_json(&["effort", "none"]);
        assert_eq!(
            state["effort_choices"]["metadata_source"],
            "Ollama /api/show"
        );
        assert_eq!(state["effort_choices"]["control"], "off_only");
        let (headers, body) = server
            .requests
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(
            headers.starts_with(&format!("POST {expected_path} HTTP/1.1")),
            "{headers}"
        );
        assert_eq!(body["model"], "qwen3-coder:30b");
        assert!(header_values(&headers, "Authorization").is_empty());
        launcher.ctl_json(&["quit"]);
    }
}

#[test]
fn server_checks_mark_profiles_and_recheck_on_demand() {
    use std::os::unix::fs::PermissionsExt;
    let server = OllamaServer::start();
    let home = server.home("toggle");
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_url = format!("http://{}/v1", closed.local_addr().unwrap());
    drop(closed);
    for (name, text) in [
        ("missing", "model='qwen3:30b-64k'\n".to_owned()),
        (
            "offline",
            format!(
                "model='toggle'\nmodel_provider='offline'\n[model_providers.offline]\nbase_url={closed_url:?}\n"
            ),
        ),
        ("wrapped", "model='toggle'\n".to_owned()),
    ] {
        std::fs::write(home.path().join(format!("{name}.config.toml")), text).unwrap();
    }
    let bin = tempfile::tempdir().unwrap();
    let wrapper = bin.path().join("codex-wrapped");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();

    let launcher = Headless::launch(home.path(), &[], bin.path(), &[]);
    let state = launcher.checked_state();
    let servers: Vec<_> = state["menu"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap(),
                entry["server"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        servers,
        [
            ("default", "ready"),
            ("missing", "model_missing"),
            ("offline", "down"),
            ("wrapped", "skipped"),
        ]
    );
    assert_eq!(
        state["profile"]["server"],
        serde_json::json!({"state": "ready", "message": null})
    );
    let state = launcher.ctl_json(&["select", "missing"]);
    assert_eq!(
        state["profile"]["server"]["message"],
        "the server has qwen3:30b"
    );
    let state = launcher.ctl_json(&["select", "wrapped"]);
    assert_eq!(
        state["profile"]["server"]["message"],
        "launched through codex-wrapped, which has no codex-wrapped.status"
    );
    launcher.ctl_json(&["select", "offline"]);
    let screen = String::from_utf8(launcher.ctl(&["screen"]).stdout).unwrap();
    for expected in [
        "│  ✓ default",
        "│  ! missing",
        "│> ✗ offline",
        "│    wrapped",
        "server    down: connection refused",
        "r recheck",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }

    // Profiles that share a server and headers share one request.
    let (headers, _) = server
        .requests
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        headers.starts_with("GET /proxy/v1/models HTTP/1.1"),
        "{headers}"
    );
    assert!(headers.contains("x-test: header-secret"), "{headers}");
    assert!(server.requests.try_recv().is_err());

    // Checking again reads the profile files again.
    std::fs::write(home.path().join("offline.config.toml"), "model='coder'\n").unwrap();
    launcher.ctl_json(&["key", "r"]);
    let state = launcher.checked_state();
    assert_eq!(state["menu"][2]["server"], "ready");
    assert_eq!(server.requests.try_iter().count(), 1);
    launcher.ctl_json(&["quit"]);
}

#[test]
fn status_commands_report_for_wrappers_without_running_them() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "model = 'base'\n").unwrap();
    let bin = tempfile::tempdir().unwrap();
    let launched = bin.path().join("launched");
    let executable = |name: String, text: String| {
        let path = bin.path().join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    for (name, report) in [
        ("broken", "not json"),
        (
            "busy",
            r#"{"state": "busy", "message": "another session holds the server"}"#,
        ),
        ("idle", r#"{"state": "starts_on_launch", "drafting": "on"}"#),
        ("up", r#"{"state": "ready", "context_window": 32768}"#),
    ] {
        std::fs::write(
            home.path().join(format!("{name}.config.toml")),
            "model = 'm'\nmodel_context_window = 65536\n",
        )
        .unwrap();
        // The wrapper would start the server, so a check must never run it.
        // The PATH holds nothing else, so it uses only shell built-ins.
        executable(
            format!("codex-{name}"),
            format!("#!/bin/sh\n: > {launched:?}\n"),
        );
        executable(
            format!("codex-{name}.status"),
            format!("#!/bin/sh\nprintf '%s' '{report}'\n"),
        );
    }

    let launcher = Headless::launch(home.path(), &[], bin.path(), &[]);
    let state = launcher.checked_state();
    let servers: Vec<_> = state["menu"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap(),
                entry["server"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        servers,
        [
            ("default", "skipped"),
            ("broken", "down"),
            ("busy", "busy"),
            ("idle", "starts_on_launch"),
            ("up", "ready"),
        ]
    );
    let state = launcher.ctl_json(&["select", "broken"]);
    let message = state["profile"]["server"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("codex-broken.status printed no valid status: "),
        "{message}"
    );
    let state = launcher.ctl_json(&["select", "busy"]);
    assert_eq!(
        state["profile"]["server"],
        serde_json::json!({"state": "busy", "message": "another session holds the server"})
    );
    let state = launcher.ctl_json(&["select", "up"]);
    assert_eq!(state["profile"]["server"]["context_window"], 32768);
    let screen = String::from_utf8(launcher.ctl(&["screen"]).stdout).unwrap();
    for expected in [
        "│  ✗ broken",
        "│  ● busy",
        "│  ○ idle",
        "│> ✓ up",
        "server    ready",
        "context   65536  server allocates 32768",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
    launcher.ctl_json(&["select", "idle"]);
    let screen = String::from_utf8(launcher.ctl(&["screen"]).stdout).unwrap();
    for expected in [
        "server    starts on launch",
        "context   65536",
        "drafting  on",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
    assert!(!screen.contains("server allocates"), "{screen}");
    assert!(!launched.exists(), "a server check ran a wrapper");
    launcher.ctl_json(&["quit"]);
}

#[test]
fn list_check_adds_server_states() {
    let server = OllamaServer::start();
    let home = server.home("toggle");
    std::fs::write(home.path().join("other.config.toml"), "model='absent'\n").unwrap();
    let run = |args: &[&str]| {
        let output = Command::new(BIN)
            .args(args)
            .env("CODEX_HOME", home.path())
            .env("PATH", "/nonexistent")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    let listing = run(&["list", "--check"]);
    let lines: Vec<_> = listing.lines().collect();
    assert!(
        lines[0].contains(" EFFORT  SERVER         DESCRIPTION"),
        "{listing}"
    );
    assert!(
        lines[1].contains(" high    ready          Runs Codex"),
        "{listing}"
    );
    assert!(lines[2].contains(" high    model missing"), "{listing}");

    let listed: Value = serde_json::from_str(&run(&["list", "--check", "--json"])).unwrap();
    assert_eq!(listed[1]["server"]["state"], "model_missing");
    assert_eq!(
        listed[1]["server"]["message"],
        "the server has toggle, coder, qwen3:30b"
    );
    let unchecked: Value = serde_json::from_str(&run(&["list", "--json"])).unwrap();
    assert!(unchecked[1].get("server").is_none());
    assert!(!run(&["list"]).contains("SERVER"));
    assert_eq!(server.requests.try_iter().count(), 2);
}
