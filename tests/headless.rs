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
        let runtime = tempfile::tempdir().unwrap();
        let child = Command::new(BIN)
            .arg("--headless")
            .args(args)
            .env("CODEX_HOME", codex_home)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("PATH", path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
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
