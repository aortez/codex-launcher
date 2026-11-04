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
        let runtime = tempfile::tempdir().unwrap();
        let child = Command::new(BIN)
            .arg("--headless")
            .args(args)
            .env("CODEX_HOME", codex_home)
            .env("XDG_RUNTIME_DIR", runtime.path())
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
