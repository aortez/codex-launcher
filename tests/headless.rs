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
    copy_tree(&fixture_home(), home.path());
    home
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn custom_harness_uses_own_commands_status_and_effort_policy() {
    use std::os::unix::fs::PermissionsExt;
    let home = writable_home();
    let bin = tempfile::tempdir().unwrap();
    let run = bin.path().join("chat");
    std::fs::write(&run, "#!/bin/sh\nif [ \"$1\" = --status ]; then\n  printf '%s' '{\"state\":\"starts_on_launch\",\"context_window\":32768}'\nelse\n  pwd\n  printf '%s\\n' \"$@\"\nfi\n").unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let codex = bin.path().join("codex");
    std::fs::write(
        &codex,
        "#!/bin/sh\nprintf 'unexpected' > \"$0.called\"\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    // `bare` leaves the descriptive fields out, so its details show what the
    // launcher cannot know rather than a Codex default it never consulted.
    let config = format!(
        "[[harness]]\nname='chat'\nharness='Playtime'\nmodel='local-model'\neffort='medium'\ntools='project Docker; no network'\nlaunch=['{}', 'fixed argument']\nstatus=['{}', '--status']\n[[harness]]\nname='bare'\nharness='Playtime'\nlaunch=['{}']\n",
        run.display(),
        run.display(),
        run.display()
    );
    std::fs::write(home.path().join("codex-launcher.toml"), &config).unwrap();
    let menu = Headless::start_with_path(home.path(), &["--", "--prompt", "hello"], bin.path());
    let state = menu.ctl_json(&["select", "chat"]);
    assert_eq!(state["profile"]["harness"], "Playtime");
    let screen = String::from_utf8(menu.ctl(&["screen"]).stdout).unwrap();
    assert!(screen.contains("harness   Playtime"), "{screen}");
    assert!(
        screen.contains("tools     project Docker; no network"),
        "{screen}"
    );
    // The preset tag says these describe the entry instead of Codex settings
    // that the launcher inherits or could override.
    for expected in [
        "model     local-model  (preset)",
        "effort    medium  (preset)",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
    assert!(
        !screen.contains("Codex default") && !screen.contains("sandbox   "),
        "{screen}"
    );
    let shown = Command::new(BIN)
        .args(["show", "bare"])
        .env("CODEX_HOME", home.path())
        .output()
        .unwrap();
    assert!(shown.status.success());
    let bare = String::from_utf8(shown.stdout).unwrap();
    assert!(
        bare.contains("model     (set by harness)") && bare.contains("effort    (set by harness)"),
        "{bare}"
    );
    let state = menu.ctl_json(&["key", "e"]);
    assert_eq!(state["effort_choices"]["metadata_source"], "harness preset");
    assert_eq!(state["effort_choices"]["levels"], serde_json::json!([]));
    assert!(!menu.ctl(&["effort", "high"]).status.success());
    assert!(!menu.ctl(&["save-effort"]).status.success());
    menu.ctl_json(&["key", "esc"]);
    let state = menu.ctl_json(&["launch"]);
    assert_eq!(
        state["profile"]["command"],
        serde_json::json!([run.to_str().unwrap(), "fixed argument", "--prompt", "hello"])
    );
    assert!(!codex.with_extension("called").exists());
    assert_eq!(
        std::fs::read_to_string(home.path().join("codex-launcher.toml")).unwrap(),
        config
    );

    let output = Command::new(BIN)
        .args(["list", "--check", "--json"])
        .env("CODEX_HOME", home.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let shown = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "chat")
        .unwrap();
    assert_eq!(shown["server"]["state"], "starts_on_launch");
    assert!(shown["sandbox"].is_null() && shown["approval"].is_null());
    let output = Command::new(BIN)
        .args(["chat", "--prompt", "literal ; $(stuff)"])
        .env("CODEX_HOME", home.path())
        .current_dir(bin.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}\nfixed argument\n--prompt\nliteral ; $(stuff)\n",
            bin.path().display()
        )
    );
}

#[test]
fn harness_dry_run_exit_status_and_launch_failures() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    // Codex is on PATH, but must never run in place of a harness.
    let codex = bin.path().join("codex");
    std::fs::write(&codex, "#!/bin/sh\nprintf 'unexpected' > \"$0.called\"\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failing = bin.path().join("failing");
    std::fs::write(&failing, "#!/bin/sh\necho \"failing $*\" >&2\nexit 7\n").unwrap();
    std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o700)).unwrap();
    let unexecutable = bin.path().join("unexecutable");
    std::fs::write(&unexecutable, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&unexecutable, std::fs::Permissions::from_mode(0o600)).unwrap();
    let absent = bin.path().join("absent");
    let entries: String = [&failing, &unexecutable, &absent]
        .iter()
        .map(|program| {
            format!(
                "[[harness]]\nname='{}'\nharness='Chat'\nlaunch=['{}', 'fixed argument']\n",
                program.file_name().unwrap().to_str().unwrap(),
                program.display()
            )
        })
        .collect();
    std::fs::write(home.path().join("codex-launcher.toml"), entries).unwrap();
    let launcher = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .env("CODEX_HOME", home.path())
            .env("PATH", bin.path())
            .output()
            .unwrap()
    };

    let output = launcher(&["failing", "--dry-run", "it's"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{} 'fixed argument' 'it'\\''s'\n", failing.display())
    );

    // The launcher becomes the harness, so it exits as the harness does.
    let output = launcher(&["failing", "--flag"]);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "failing fixed argument --flag\n"
    );

    for (program, problem) in [
        (&unexecutable, "Permission denied (os error 13)"),
        (&absent, "No such file or directory (os error 2)"),
    ] {
        let name = program.file_name().unwrap().to_str().unwrap();
        let output = launcher(&[name]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!(
                "codex-launcher: cannot run {}: {problem}\n",
                program.display()
            )
        );
    }
    assert!(!codex.with_extension("called").exists());
}

#[test]
fn the_menu_hands_its_terminal_to_a_harness() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// A child that the test kills if it ends early.
    struct Reaped(Child);
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // script from util-linux runs the menu in a pseudo-terminal. Other
    // versions take other options. The test skips only off Linux, so that CI,
    // which runs on Linux, cannot pass it without running it.
    let version = Command::new("script").arg("--version").output();
    if !version.is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("util-linux")) {
        if cfg!(target_os = "linux") {
            panic!("this test needs script from util-linux");
        }
        eprintln!("skipped: this test needs script from util-linux");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let report = bin.path().join("report");
    let chat = bin.path().join("chat");
    // Report where the harness runs, then wait for Ctrl-C. The report is
    // renamed into place, so the test never reads part of it. Some versions
    // of tty print no newline, so echo adds one.
    std::fs::write(
        &chat,
        "#!/bin/sh\nPATH=/usr/bin:/bin\n{ echo $$; echo \"$(tty)\"; pwd; printf '%s\\n' \"$@\"; stty -a; } > \"$REPORT.part\"\nmv \"$REPORT.part\" \"$REPORT\"\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&chat, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.path().join("codex-launcher.toml"),
        format!(
            "[[harness]]\nname='chat'\nharness='Chat'\nlaunch=['{}', 'fixed argument']\n",
            chat.display()
        ),
    )
    .unwrap();
    // A new pseudo-terminal has no size until something sets one. As in the
    // other tests, the menu finds no programs on PATH.
    let menu = format!(
        "stty cols 100 rows 30 && exec env PATH=/nonexistent '{BIN}' --no-check -- 'literal ; $(x)'"
    );
    let mut script = Reaped(
        Command::new("script")
            .args(["--quiet", "--return", "--command", &menu, "/dev/null"])
            .current_dir(project.path())
            .env("CODEX_HOME", home.path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("REPORT", &report)
            // script runs the command with $SHELL.
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let sockets = runtime.path().join("codex-launcher");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_dir(&sockets).is_ok_and(|mut entries| entries.next().is_some()) {
        assert!(Instant::now() < deadline, "the menu never listened");
        std::thread::sleep(Duration::from_millis(10));
    }
    let ctl = |args: &[&str]| -> Value {
        let output = Command::new(BIN)
            .arg("ctl")
            .args(args)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    assert_eq!(ctl(&["select", "chat"])["mode"], "tui");
    let state = ctl(&["launch"]);
    assert_eq!(state["status"], "launching");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !report.exists() {
        assert!(Instant::now() < deadline, "the harness never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    let text = std::fs::read_to_string(&report).unwrap();
    let mut lines = text.lines();
    // The harness replaced the menu's process, so it has the menu's terminal
    // and directory.
    assert_eq!(
        lines.next(),
        Some(state["pid"].to_string().as_str()),
        "{text}"
    );
    assert_eq!(lines.next(), state["tty"].as_str(), "{text}");
    assert_eq!(lines.next(), project.path().to_str(), "{text}");
    assert_eq!(lines.next(), Some("fixed argument"));
    assert_eq!(lines.next(), Some("literal ; $(x)"));
    // The menu restored the terminal's line settings before the handoff.
    let settings: Vec<&str> = lines.flat_map(str::split_whitespace).collect();
    for setting in ["icanon", "echo", "isig"] {
        assert!(settings.contains(&setting), "{setting} is off:\n{text}");
    }

    // Ctrl-C at the terminal interrupts the harness, which script reports as
    // a shell does: 128 plus SIGINT.
    let stdin = script.0.stdin.as_mut().unwrap();
    stdin.write_all(b"\x03").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = script.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Ctrl-C did not interrupt the harness"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(130));
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

    /// Start with server checks off, in the fixture sessions' project, on a
    /// screen that shows every fixture session.
    fn start_in_project(sessions: &SessionsFixture, args: &[&str]) -> Self {
        Self::start_in_project_sized(sessions, "120x40", args)
    }

    fn start_in_project_sized(sessions: &SessionsFixture, size: &str, args: &[&str]) -> Self {
        let args: Vec<&str> = ["--no-check", "--size", size]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        Self::launch_in(
            sessions.home.path(),
            &args,
            Path::new("/nonexistent"),
            &[],
            Some(sessions.project.path()),
        )
    }

    fn launch(codex_home: &Path, args: &[&str], path: &Path, env: &[(&str, Option<&str>)]) -> Self {
        Self::launch_in(codex_home, args, path, env, None)
    }

    fn launch_in(
        codex_home: &Path,
        args: &[&str],
        path: &Path,
        env: &[(&str, Option<&str>)],
        dir: Option<&Path>,
    ) -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let mut command = Command::new(BIN);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
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

    /// The state once the sessions have been read.
    fn sessions_state(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = self.ctl_json(&["state"]);
            if state["sessions"]["loading"] == false {
                return state;
            }
            assert!(Instant::now() < deadline, "the sessions never loaded");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Select the session on screen with this title, and return the state.
    fn select_session(&self, title: &str) -> Value {
        let state = self.sessions_state();
        let list = state["sessions"]["list"].as_array().unwrap();
        let row = shown_from(&state)
            + list
                .iter()
                .position(|row| row["title"] == title)
                .unwrap_or_else(|| panic!("no session {title:?} in {list:?}"));
        let keys: Vec<&str> = std::iter::once("home")
            .chain(std::iter::repeat_n("down", row))
            .collect();
        let state = self.ctl_json(&std::iter::once("key").chain(keys).collect::<Vec<_>>());
        assert_eq!(state["sessions"]["selected_index"], row);
        assert_eq!(
            state["sessions"]["list"][row - shown_from(&state)]["title"],
            title
        );
        state
    }

    fn screen(&self) -> String {
        String::from_utf8(self.ctl(&["screen"]).stdout).unwrap()
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
        "launched through codex-wrapped, and no codex-wrapped.status is on PATH"
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
    let allocates = r#"{"state": "ready", "context_window": 32768}"#;
    for (name, context, report) in [
        ("auto", None, allocates),
        ("broken", Some(65536), "not json"),
        (
            "busy",
            Some(65536),
            r#"{"state": "busy", "message": "another session holds the server"}"#,
        ),
        ("even", Some(32768), allocates),
        (
            "idle",
            Some(65536),
            r#"{"state": "starts_on_launch", "drafting": "on"}"#,
        ),
        ("up", Some(65536), allocates),
    ] {
        let mut config = "model = 'm'\n".to_owned();
        if let Some(tokens) = context {
            config.push_str(&format!("model_context_window = {tokens}\n"));
        }
        std::fs::write(home.path().join(format!("{name}.config.toml")), config).unwrap();
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
            ("auto", "ready"),
            ("broken", "unknown"),
            ("busy", "busy"),
            ("even", "ready"),
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
    let screen_of = |profile: &str| {
        launcher.ctl_json(&["select", profile]);
        String::from_utf8(launcher.ctl(&["screen"]).stdout).unwrap()
    };
    let status_path = bin.path().join("codex-up.status");
    let state = launcher.ctl_json(&["select", "up"]);
    assert_eq!(state["profile"]["server"]["context_window"], 32768);
    assert_eq!(
        state["profile"]["status_command"],
        status_path.to_str().unwrap()
    );
    let screen = screen_of("up");
    for expected in [
        "│  ? broken".to_owned(),
        "│  ◉ busy".to_owned(),
        "│  ◌ idle".to_owned(),
        "│> ✓ up".to_owned(),
        "server    ready".to_owned(),
        "context   65536  server allocates 32768".to_owned(),
        format!("status    {}", status_path.display()),
    ] {
        assert!(
            screen.contains(&expected),
            "{expected:?} missing:\n{screen}"
        );
    }
    let screen = screen_of("auto");
    assert!(
        screen.contains("context   (Codex default)  server allocates 32768"),
        "{screen}"
    );
    let screen = screen_of("even");
    assert!(screen.contains("context   32768"), "{screen}");
    assert!(!screen.contains("server allocates"), "{screen}");
    let screen = screen_of("idle");
    for expected in [
        "server    starts on launch",
        "context   65536",
        "drafting  on",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
    assert!(!screen.contains("server allocates"), "{screen}");
    launcher.ctl_json(&["quit"]);

    // With another provider, Codex would not use the server the command checks.
    let launcher = Headless::launch(home.path(), &["--", "--oss"], bin.path(), &[]);
    launcher.checked_state();
    let state = launcher.ctl_json(&["select", "up"]);
    assert_eq!(
        state["profile"]["server"],
        serde_json::json!({"state": "skipped", "message": "launch arguments select the profile or provider"})
    );
    launcher.ctl_json(&["quit"]);

    let output = Command::new(BIN)
        .args(["list", "--check", "--json"])
        .env("CODEX_HOME", home.path())
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let server_of = |profile: &str| {
        let listed = listed.as_array().unwrap();
        listed
            .iter()
            .find(|entry| entry["name"] == profile)
            .unwrap()["server"]
            .clone()
    };
    assert_eq!(
        server_of("idle"),
        serde_json::json!({"state": "starts_on_launch", "message": null, "drafting": "on"})
    );
    assert_eq!(
        server_of("up"),
        serde_json::json!({"state": "ready", "message": null, "context_window": 32768})
    );
    assert!(!launched.exists(), "a server check ran a wrapper");
}

#[test]
fn host_status_is_shared_but_reports_each_profile_and_refreshes() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "model = 'base'\n").unwrap();
    for name in ["alpha", "beta", "missing", "other"] {
        std::fs::write(
            home.path().join(format!("{name}.config.toml")),
            "model = 'm'\n",
        )
        .unwrap();
    }
    let helper = bin.path().join("status");
    std::fs::write(&helper, r#"#!/bin/sh
printf 'check\n' >> "$0.$1.calls"
if [ "$1" = first ]; then
  printf '%s' '{"profiles":{"alpha":{"state":"starts_on_launch","context_window":131072,"drafting":"off"},"beta":{"state":"busy","message":"another session"}}}'
else
  printf '%s' '{"profiles":{"other":{"state":"down","message":"host unreachable"}}}'
fi
"#).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A launch command must not run during checks, even for a ready host.
    let launch = bin.path().join("launch");
    std::fs::write(&launch, "#!/bin/sh\nprintf 'launched' > \"$0.calls\"\n").unwrap();
    std::fs::set_permissions(&launch, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.path().join("codex-launcher.toml"),
        format!(
            r#"
[[wrapper]]
profiles = ["alpha", "beta", "missing"]
launch = [{launch:?}, "{{profile}}"]
status = [{helper:?}, "first"]
[[wrapper]]
profiles = ["other"]
launch = [{launch:?}, "{{profile}}"]
status = [{helper:?}, "second"]
"#
        ),
    )
    .unwrap();
    let launcher = Headless::start_with_path(home.path(), &[], bin.path());
    assert!(!bin.path().join("status.first.calls").exists());
    for count in 1..=2 {
        launcher.ctl_json(&["key", "r"]);
        launcher.checked_state();
        for host in ["first", "second"] {
            let calls =
                std::fs::read_to_string(bin.path().join(format!("status.{host}.calls"))).unwrap();
            assert_eq!(
                calls.lines().count(),
                count,
                "one check per host per refresh"
            );
        }
        for (name, expected) in [
            ("alpha", "starts_on_launch"),
            ("beta", "busy"),
            ("missing", "unknown"),
            ("other", "down"),
        ] {
            let state = launcher.ctl_json(&["select", name]);
            assert_eq!(state["profile"]["server"]["state"], expected);
        }
        let state = launcher.ctl_json(&["select", "missing"]);
        assert_eq!(
            state["profile"]["server"]["message"],
            "status report omitted missing"
        );
        let state = launcher.ctl_json(&["select", "alpha"]);
        assert_eq!(state["profile"]["server"]["context_window"], 131072);
        assert_eq!(state["profile"]["server"]["drafting"], "off");
    }
    assert!(!bin.path().join("launch.calls").exists());
    launcher.ctl_json(&["quit"]);
}

#[test]
fn codex_launcher_toml_names_launch_and_status_commands() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let codex_home = home.path().join(".codex");
    std::fs::create_dir(&codex_home).unwrap();
    for name in ["alpha", "beta", "plain"] {
        std::fs::write(
            codex_home.join(format!("{name}.config.toml")),
            "model = 'm'\n",
        )
        .unwrap();
    }
    let launched = home.path().join("launched");
    let executable = |path: PathBuf, text: String| {
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    // One script serves every profile, and reports instead with --status. The
    // PATH holds nothing else, so it uses only shell built-ins.
    std::fs::create_dir(home.path().join("bin")).unwrap();
    let run = home.path().join("bin/run");
    executable(
        run.clone(),
        format!(
            "#!/bin/sh\nif [ \"$1\" = --status ]; then\n  printf '{{\"state\": \"busy\", \"message\": \"%s is busy\"}}' \"$2\"\nelse\n  : > {launched:?}\nfi\n"
        ),
    );
    // The file takes precedence over these, even where its commands are broken.
    let bin = tempfile::tempdir().unwrap();
    for name in ["codex-alpha", "codex-beta"] {
        executable(
            bin.path().join(name),
            format!("#!/bin/sh\n: > {launched:?}\n"),
        );
    }
    executable(
        bin.path().join("codex-alpha.status"),
        "#!/bin/sh\nprintf '{\"state\": \"ready\"}'\n".to_owned(),
    );
    let config = codex_home.join("codex-launcher.toml");
    std::fs::write(
        &config,
        r#"
[[wrapper]]
profiles = ["alpha", "missing"]
launch = ["~/bin/run", "{profile}"]
status = ["~/bin/run", "--status", "{profile}"]

[[wrapper]]
profiles = ["beta"]
launch = ["~/bin/run", "{profile}"]
status = ["bin/run", "--status", "{profile}"]

[[wrapper]]
profiles = ["plain"]
launch = ["~/bin/absent"]
"#,
    )
    .unwrap();
    let launcher_with_home = |args: &[&str], home_var: &Path| {
        Command::new(BIN)
            .args(args)
            .current_dir(home.path())
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home_var)
            .env("PATH", bin.path())
            .output()
            .unwrap()
    };
    let launcher = |args: &[&str]| launcher_with_home(args, home.path());
    let warnings = "codex-launcher: warning: codex-launcher.toml: wrapper 1: no profile is named \"missing\"\n\
        codex-launcher: warning: codex-launcher.toml: wrapper 2: status must start with an absolute path or ~/, not \"bin/run\"\n";

    let output = launcher(&["list", "--check", "--json"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stderr).unwrap(), warnings);
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let entry_in = |listed: &Value, profile: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == profile)
            .unwrap()
            .clone()
    };
    let entry = |profile: &str| entry_in(&listed, profile);
    let run_path = run.to_str().unwrap();
    let alpha = entry("alpha");
    assert_eq!(
        alpha["server"],
        serde_json::json!({"state": "busy", "message": "alpha is busy"})
    );
    assert_eq!(alpha["command"], serde_json::json!([run_path, "alpha"]));
    assert_eq!(alpha["wrapper"], run_path);
    assert_eq!(
        alpha["status_command"],
        format!("{run_path} --status alpha")
    );
    // Its broken status command is left out, but codex-beta still is not used.
    let beta = entry("beta");
    assert_eq!(beta["command"], serde_json::json!([run_path, "beta"]));
    assert_eq!(
        beta["server"],
        serde_json::json!({"state": "skipped", "message": "launched through run, and codex-launcher.toml gives it no status command"})
    );

    // Launching directly or showing a profile prints the warnings too.
    let output = launcher(&["beta", "--dry-run"]);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{run_path} beta\n")
    );
    assert_eq!(String::from_utf8(output.stderr).unwrap(), warnings);
    let output = launcher(&["show", "beta"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stderr).unwrap(), warnings);
    let output = launcher(&["plain"]);
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "{warnings}codex-launcher: cannot run {}: No such file or directory (os error 2)\n",
            home.path().join("bin/absent").display()
        )
    );

    // A relative HOME would make ~/ depend on the current directory, which
    // here holds bin/run.
    let output = launcher_with_home(&["list", "--check", "--json"], Path::new("."));
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for key in ["launch", "status"] {
        let warning = format!("wrapper 1: {key} starts with ~/, but HOME is unset or not absolute");
        assert!(stderr.contains(&warning), "{warning:?} missing:\n{stderr}");
    }
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let alpha = entry_in(&listed, "alpha");
    assert_eq!(
        alpha["command"],
        serde_json::json!(["codex", "--profile", "alpha"])
    );
    assert_eq!(alpha["status_command"], Value::Null);

    // An effort override goes after the wrapper's own arguments.
    let menu = Headless::launch(
        &codex_home,
        &["--no-check", "--", "exec", "hi"],
        bin.path(),
        &[("HOME", home.path().to_str())],
    );
    menu.ctl_json(&["select", "alpha"]);
    let state = menu.ctl_json(&["effort", "high"]);
    assert_eq!(
        state["profile"]["command"],
        serde_json::json!([
            run_path,
            "alpha",
            "-c",
            "model_reasoning_effort=\"high\"",
            "exec",
            "hi"
        ])
    );
    let screen_has = |expected: &[&str]| {
        let screen = String::from_utf8(menu.ctl(&["screen"]).stdout).unwrap();
        for expected in expected {
            assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
        }
        assert!(!screen.contains("wrapper "), "{screen}");
    };
    screen_has(&[
        "launch    ~/bin/run alpha -c 'model_reasoning_effort=\"high\"' exec hi",
        "status    ~/bin/run --status alpha",
    ]);
    // Saving reloads the profiles, but what runs and what shows still come
    // from the file as it was when the menu opened.
    std::fs::remove_file(&config).unwrap();
    let state = menu.ctl_json(&["save-effort"]);
    assert_eq!(
        state["profile"]["command"],
        serde_json::json!([run_path, "alpha", "exec", "hi"])
    );
    screen_has(&[
        "launch    ~/bin/run alpha exec hi",
        "status    ~/bin/run --status alpha",
    ]);
    menu.ctl_json(&["launch"]);
    assert_eq!(menu.finish(), format!("{run_path} alpha exec hi\n"));
    let saved = std::fs::read_to_string(codex_home.join("alpha.config.toml")).unwrap();
    assert!(
        saved.contains("model_reasoning_effort = \"high\""),
        "{saved}"
    );
    assert!(!launched.exists(), "a server check ran a wrapper");
}

#[test]
fn quitting_stops_status_commands_that_are_still_running() {
    use rustix::process::{Pid, Signal, kill_process};
    use std::os::unix::fs::PermissionsExt;

    // Kill the sleeper even if an assertion fails, so a regression does not
    // leave it running.
    struct Sleeper(Pid);
    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = kill_process(self.0, Signal::KILL);
        }
    }

    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("slow.config.toml"), "model = 'm'\n").unwrap();
    let bin = tempfile::tempdir().unwrap();
    let pid_file = bin.path().join("sleeper.pid");
    let status = bin.path().join("codex-slow.status");
    std::fs::write(
        &status,
        format!("#!/bin/sh\n/bin/sleep 60 &\nprintf '%s' \"$!\" > {pid_file:?}\nwait\n"),
    )
    .unwrap();
    std::fs::set_permissions(&status, std::fs::Permissions::from_mode(0o700)).unwrap();

    let launcher = Headless::launch(home.path(), &[], bin.path(), &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.parse().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the status command never ran");
        std::thread::sleep(Duration::from_millis(10));
    };
    let _sleeper = Sleeper(Pid::from_raw(pid).unwrap());
    launcher.ctl_json(&["quit"]);
    launcher.finish();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"));
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
            "status command descendant {pid} outlived the menu"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
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

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// How long before the test each fixture log last changed, by the start of
/// its path under sessions/2026/11. Git keeps no modification times.
const SESSION_AGES: &[(&str, u64)] = &[
    ("01/rollout-2026-11-01T10-00-00", 30 * DAY),
    ("07/rollout-2026-11-07T11-00-00", 15 * DAY),
    ("19/rollout-2026-11-19T10-00-00", 3 * DAY + HOUR),
    ("21/rollout-2026-11-21T07-00-00", DAY + 5 * HOUR),
    ("21/rollout-2026-11-21T09-30-00", DAY + 2 * HOUR),
    ("22/rollout-2026-11-22T06-20-00", 5 * HOUR + 30 * MINUTE),
    ("22/rollout-2026-11-22T07-40-00", 4 * HOUR + 20 * MINUTE),
    ("22/rollout-2026-11-22T08-30-00", 3 * HOUR + 15 * MINUTE),
    ("22/rollout-2026-11-22T09-40-00", 2 * HOUR + 10 * MINUTE),
    ("22/rollout-2026-11-22T10-30-00", HOUR + 30 * MINUTE),
    ("22/rollout-2026-11-22T11-30-00", 20 * MINUTE),
    ("22/rollout-2026-11-22T11-40-00", 10 * MINUTE),
];

/// The directory that the fixture sessions record for their project.
const FIXTURE_PROJECT: &str = "/home/user/project";
/// Text in every fixture transcript, which the launcher must never show.
const TRANSCRIPT: &str = "Transcript text that the launcher never shows.";
/// A second profile on the fixture gateway with the same model.
const GATEWAY_BACKUP: &str = "model = \"gpt-team\"\nmodel_provider = \"gateway\"\n\n[model_providers.gateway]\nname = \"Backup gateway\"\nbase_url = \"https://backup.example.com/v1\"\n";
/// A provider that config.toml defines and no profile uses.
const LAB_PROVIDER: &str =
    "\n[model_providers.lab]\nname = \"Lab\"\nbase_url = \"https://lab.example.com/v1\"\n";

fn session_id(n: u16) -> String {
    format!("019a0000-0000-7000-8000-{n:012x}")
}

/// A copy of the fixture home whose project sessions ran in a temporary
/// project directory instead, so that a launcher started there lists them as
/// its directory's.
struct SessionsFixture {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl SessionsFixture {
    fn new() -> Self {
        let home = writable_home();
        let project = tempfile::tempdir().unwrap();
        let quoted = |path: &str| serde_json::to_string(path).unwrap();
        let (from, to) = (
            quoted(FIXTURE_PROJECT),
            quoted(project.path().to_str().unwrap()),
        );
        let month = home.path().join("sessions/2026/11");
        let mut logs = Vec::new();
        for day in std::fs::read_dir(&month).unwrap() {
            for log in std::fs::read_dir(day.unwrap().path()).unwrap() {
                logs.push(log.unwrap().path());
            }
        }
        assert_eq!(logs.len(), SESSION_AGES.len());
        for log in logs {
            let relative = log.strip_prefix(&month).unwrap().to_str().unwrap();
            let age = SESSION_AGES
                .iter()
                .find(|(prefix, _)| relative.starts_with(prefix))
                .unwrap_or_else(|| panic!("no age for {relative}"))
                .1;
            let compressed = relative.ends_with(".zst");
            let mut bytes = std::fs::read(&log).unwrap();
            if compressed {
                bytes = zstd::decode_all(bytes.as_slice()).unwrap();
            }
            let mut bytes = String::from_utf8(bytes)
                .unwrap()
                .replace(&from, &to)
                .into_bytes();
            if compressed {
                bytes = zstd::encode_all(bytes.as_slice(), 0).unwrap();
            }
            std::fs::write(&log, bytes).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&log)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(age))
                .unwrap();
        }
        Self { home, project }
    }

    /// Add a second gateway profile, and a provider that only config.toml
    /// defines.
    fn with_backup_and_lab(self) -> Self {
        let home = self.home.path();
        std::fs::write(home.join("gateway-backup.config.toml"), GATEWAY_BACKUP).unwrap();
        let config = home.join("config.toml");
        let base = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, base + LAB_PROVIDER).unwrap();
        self
    }
}

/// Each listed session as "title | model | profiles".
fn session_rows(state: &Value) -> Vec<String> {
    state["sessions"]["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let profiles: Vec<&str> = row["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .map(|profile| profile.as_str().unwrap())
                .collect();
            format!(
                "{} | {} | {}",
                row["title"].as_str().unwrap(),
                row["model"].as_str().unwrap_or("-"),
                profiles.join(", ")
            )
        })
        .collect()
}

/// The row that the state's list of sessions starts with.
fn shown_from(state: &Value) -> usize {
    usize::try_from(state["sessions"]["offset"].as_u64().unwrap()).unwrap()
}

fn assert_shows(screen: &str, expected: &[&str]) {
    for expected in expected {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
}

#[test]
fn sessions_view_lists_this_directory_then_every_directory() {
    let sessions = SessionsFixture::new();
    let menu = Headless::start_in_project(&sessions, &[]);
    // The menu reads no session logs until the view opens.
    let state = menu.ctl_json(&["state"]);
    assert_eq!(state["view"], "profiles");
    assert!(state["sessions"].is_null(), "{state}");
    assert_eq!(menu.ctl_json(&["key", "tab"])["view"], "sessions");

    let state = menu.sessions_state();
    assert_eq!(state["sessions"]["scope"], "directory");
    // Newest first, without the exec run and the subagent thread. The local
    // session's newer log is its second, after a model switch, but its model
    // is its first turn's. The unnamed sessions show their branch or ID.
    let id4 = session_id(4);
    assert_eq!(
        session_rows(&state),
        [
            "Fix the flaky tests | gpt-team | gateway",
            "Try the local model | qwen3-coder:30b | local",
            "unknown | - | ",
            "docs | gpt-base | default",
            &format!("{id4} | gpt-team | gateway"),
        ]
    );
    let list = &state["sessions"]["list"];
    assert_eq!(list[1]["logs"].as_array().unwrap().len(), 2);
    assert!(list[3]["logs"][0].as_str().unwrap().ends_with(".jsonl.zst"));
    assert_eq!(list[2]["error"], "session_meta records no model_provider");
    assert_eq!(
        state["sessions"]["warnings"],
        serde_json::json!(["2 session logs could not be read"])
    );
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "--profile", "gateway", "resume", session_id(1)])
    );
    let screen = menu.screen();
    assert_shows(
        &screen,
        &[
            " Sessions · ",
            "Name",
            "Updated",
            "Model",
            "Profile",
            "> Fix the flaky tests",
            "2h ago",
            "3h ago",
            "4h ago",
            "1d ago",
            "3d ago",
            " 2 session logs could not be read ",
            "name      Fix the flaky tests",
            "branch    main",
            "updated   2h ago · ",
            "provider  gateway",
            "profile   gateway",
            &format!("launch    codex --profile gateway resume {}", session_id(1)),
            "Tab profiles · a every directory · Enter resume",
        ],
    );
    assert!(!screen.contains("Directory"), "{screen}");

    let state = menu.ctl_json(&["key", "a"]);
    assert_eq!(state["sessions"]["scope"], "all");
    assert_eq!(
        session_rows(&state),
        [
            "unknown | - | ",
            "Fix the flaky tests | gpt-team | gateway",
            "Try the local model | qwen3-coder:30b | local",
            "unknown | - | ",
            "Review the docs | gpt-team-mini | gateway",
            "docs | gpt-base | default",
            &format!("{id4} | gpt-team | gateway"),
            "Lab notes | lab-model | ",
            "Review the docs | gpt-base | default",
        ]
    );
    // The selection stays on the same session.
    assert_eq!(state["sessions"]["selected_index"], 1);
    assert_shows(
        &menu.screen(),
        &[
            " Sessions · every directory ",
            "Directory",
            "a this directory",
        ],
    );
    // A log whose first line does not parse has no known directory, so only
    // the list of every directory shows it.
    menu.select_session("unknown");
    assert_shows(
        &menu.screen(),
        &[
            "directory unknown",
            "error     the first record does not parse",
            "updated   1h ago · ",
        ],
    );
    let state = menu.ctl_json(&["key", "enter"]);
    assert_eq!(state["status"], "selecting");
    assert_eq!(
        state["notice"],
        "This log could not be read, so it cannot be resumed."
    );
    assert!(state["command"].is_null());

    let state = String::from_utf8(menu.ctl(&["state"]).stdout).unwrap();
    assert!(!state.contains(TRANSCRIPT) && !menu.screen().contains(TRANSCRIPT));
    menu.ctl_json(&["quit"]);
}

#[test]
fn session_details_say_which_profile_resumes_each_session() {
    let sessions = SessionsFixture::new();
    let menu = Headless::start_in_project(&sessions, &[]);
    menu.ctl_json(&["key", "tab", "a"]);

    menu.select_session("Review the docs");
    assert_shows(
        &menu.screen(),
        &[
            "model     gpt-team-mini  gateway uses gpt-team",
            "profile   gateway",
        ],
    );
    let state = menu.select_session("Lab notes");
    assert!(state["command"].is_null());
    assert_shows(
        &menu.screen(),
        &[
            "> Lab notes",
            "none",
            "profile   none  needs a profile that uses provider lab",
        ],
    );
    let state = menu.ctl_json(&["key", "enter"]);
    assert_eq!(state["status"], "selecting");
    assert_eq!(
        state["notice"],
        "No profile can resume this session: it needs provider lab."
    );
    let id4 = session_id(4);
    menu.select_session(&id4);
    let screen = menu.screen();
    assert_shows(&screen, &[&format!("id        {id4}")]);
    assert!(!screen.contains("branch    "), "{screen}");
    menu.select_session("docs");
    assert_shows(&menu.screen(), &["branch    docs", "profile   default"]);
    menu.ctl_json(&["quit"]);

    // With a second gateway profile, two profiles match the gateway sessions,
    // and config.toml now defines the lab provider for plain `codex resume`.
    let sessions = SessionsFixture::new().with_backup_and_lab();
    let menu = Headless::start_in_project(&sessions, &[]);
    menu.ctl_json(&["key", "tab", "a"]);
    menu.select_session("Lab notes");
    assert_shows(
        &menu.screen(),
        &[
            "model     lab-model  default uses gpt-base",
            "profile   default  (config.toml defines lab)",
            &format!("launch    codex resume {}", session_id(6)),
        ],
    );
    menu.select_session("Review the docs");
    assert_shows(
        &menu.screen(),
        &["model     gpt-team-mini  the matching profiles use other models"],
    );
    let state = menu.select_session("Fix the flaky tests");
    assert!(state["command"].is_null());
    assert_shows(
        &menu.screen(),
        &["profile   gateway, gateway-backup  (Enter asks which)"],
    );
    let state = menu.ctl_json(&["key", "enter"]);
    assert_eq!(
        state["profile_picker"],
        serde_json::json!({"session": session_id(1), "profiles": ["gateway", "gateway-backup"], "selected": 0})
    );
    assert_shows(
        &menu.screen(),
        &[
            " Resume with ",
            "Several profiles can resume \"Fix the flaky tests\".",
            "> gateway         gpt-team",
            "gateway-backup  gpt-team",
            "↑/↓ choose · Enter resume · Esc cancel",
        ],
    );
    assert!(menu.ctl_json(&["key", "esc"])["profile_picker"].is_null());
    let state = menu.ctl_json(&["key", "enter", "down"]);
    assert_eq!(
        state["command"],
        serde_json::json!([
            "codex",
            "--profile",
            "gateway-backup",
            "resume",
            session_id(1)
        ])
    );
    menu.ctl_json(&["key", "enter"]);
    assert_eq!(
        menu.finish(),
        format!("codex --profile gateway-backup resume {}\n", session_id(1))
    );
}

#[test]
fn launching_a_session_resumes_it_through_its_profile() {
    let sessions = SessionsFixture::new();
    std::fs::write(
        sessions.home.path().join("codex-launcher.toml"),
        "[[wrapper]]\nprofiles = [\"gateway\"]\nlaunch = [\"/opt/gateway/run\", \"{profile}\"]\n",
    )
    .unwrap();
    let menu = Headless::start_in_project(&sessions, &[]);
    menu.ctl_json(&["key", "tab"]);
    let state = menu.sessions_state();
    let id1 = session_id(1);
    assert_eq!(
        state["command"],
        serde_json::json!(["/opt/gateway/run", "gateway", "resume", id1])
    );
    assert_shows(
        &menu.screen(),
        &[&format!("launch    /opt/gateway/run gateway resume {id1}")],
    );
    assert_eq!(menu.ctl_json(&["key", "enter"])["status"], "launching");
    assert_eq!(
        menu.finish(),
        format!("/opt/gateway/run gateway resume {id1}\n")
    );

    // A bare `resume` opens the sessions view, and the command keeps it.
    let menu = Headless::start_in_project(&sessions, &["--", "resume"]);
    assert_eq!(menu.sessions_state()["view"], "sessions");
    let state = menu.select_session("docs");
    let id2 = session_id(2);
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "resume", id2])
    );
    // In the profiles view, Enter still leaves the choice to Codex.
    let state = menu.ctl_json(&["key", "tab"]);
    assert_eq!(state["command"], serde_json::json!(["codex", "resume"]));
    menu.ctl_json(&["key", "tab", "enter"]);
    assert_eq!(menu.finish(), format!("codex resume {id2}\n"));
}

#[test]
fn sessions_resume_only_without_other_launch_arguments() {
    let sessions = SessionsFixture::new();
    for args in [&["--", "exec", "fix tests"][..], &["--", "--search"]] {
        let menu = Headless::start_in_project(&sessions, args);
        menu.ctl_json(&["key", "tab"]);
        let state = menu.sessions_state();
        assert!(state["command"].is_null(), "{args:?}");
        assert_shows(
            &menu.screen(),
            &["launch    not with these launch arguments; give none, or a bare \"resume\""],
        );
        let state = menu.ctl_json(&["key", "enter"]);
        assert_eq!(state["status"], "selecting", "{args:?}");
        assert_eq!(
            state["notice"],
            "Sessions resume only without launch arguments, or with a bare \"resume\"."
        );
        // Launching a profile still passes the arguments on.
        menu.ctl_json(&["key", "tab", "enter"]);
        let expected = if args.len() == 3 {
            "codex exec 'fix tests'\n"
        } else {
            "codex --search\n"
        };
        assert_eq!(menu.finish(), expected);
    }
}

/// Start a menu in another directory with `-- resume <target>`, and wait
/// for it to find the session.
fn resumed(home: &Path, target: &str) -> (Headless, Value) {
    let menu = Headless::start_in(home, &["--", "resume", target]);
    let state = menu.sessions_state();
    (menu, state)
}

#[test]
fn resume_arguments_select_the_sessions_profile() {
    let sessions = SessionsFixture::new();
    // The session's directory does not matter.
    let id1 = session_id(1);
    let (menu, state) = resumed(sessions.home.path(), &id1);
    assert_eq!(state["view"], "profiles");
    assert_eq!(state["selected"], "gateway");
    assert_eq!(
        state["notice"],
        "Selected gateway for session \"Fix the flaky tests\"."
    );
    menu.ctl_json(&["launch"]);
    assert_eq!(
        menu.finish(),
        format!("codex --profile gateway resume {id1}\n")
    );
    let (_menu, state) = resumed(sessions.home.path(), "Try the local model");
    assert_eq!(state["selected"], "local");

    // Codex may know a session or provider that the launcher does not.
    for (target, notice) in [
        (
            "Lab notes",
            "No profile can resume \"Lab notes\": it needs provider lab.",
        ),
        (
            "019a0000-0000-7000-8000-0000000000ff",
            "No session has ID 019a0000-0000-7000-8000-0000000000ff.",
        ),
    ] {
        let (_menu, state) = resumed(sessions.home.path(), target);
        assert_eq!(state["view"], "profiles", "{target}");
        assert_eq!(state["selected"], "default", "{target}");
        assert_eq!(state["notice"], notice, "{target}");
        assert_eq!(
            state["command"],
            serde_json::json!(["codex", "resume", target])
        );
    }

    // When two profiles can resume the session, the menu selects the first
    // and asks.
    let sessions = sessions.with_backup_and_lab();
    let (menu, state) = resumed(sessions.home.path(), &id1);
    assert_eq!(state["selected"], "gateway");
    assert!(state["notice"].is_null(), "{state}");
    assert_eq!(
        state["profile_picker"],
        serde_json::json!({"session": id1, "profiles": ["gateway", "gateway-backup"], "selected": 0})
    );
    assert_shows(
        &menu.screen(),
        &[
            " Resume with ",
            "Several profiles can resume \"Fix the flaky tests\".",
        ],
    );
    let state = menu.ctl_json(&["key", "down"]);
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "--profile", "gateway-backup", "resume", id1])
    );
    // Dismissing the picker leaves the first selected.
    let state = menu.ctl_json(&["key", "esc"]);
    assert!(state["profile_picker"].is_null());
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "--profile", "gateway", "resume", id1])
    );
    let (menu, _) = resumed(sessions.home.path(), &id1);
    let state = menu.ctl_json(&["key", "down", "enter"]);
    assert_eq!(state["status"], "launching");
    assert_eq!(state["selected"], "gateway-backup");
    assert_eq!(
        menu.finish(),
        format!("codex --profile gateway-backup resume {id1}\n")
    );
}

#[test]
fn a_name_that_sessions_share_lists_them_to_choose_from() {
    let sessions = SessionsFixture::new();
    let (id1, id5, id10) = (session_id(1), session_id(5), session_id(10));
    let (menu, state) = resumed(sessions.home.path(), "Review the docs");
    assert_eq!(state["view"], "sessions");
    assert_eq!(
        state["notice"],
        "2 sessions are named \"Review the docs\"; choose one."
    );
    let listed = &state["sessions"];
    assert_eq!(listed["scope"], "named");
    assert_eq!(listed["total"], 2);
    assert_eq!(
        session_rows(&state),
        [
            "Review the docs | gpt-team-mini | gateway",
            "Review the docs | gpt-base | default",
        ]
    );
    // The chosen session resumes by its ID, in place of the name.
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "--profile", "gateway", "resume", id5])
    );
    assert_shows(
        &menu.screen(),
        &[" Sessions · named \"Review the docs\" ", "Directory"],
    );
    let state = menu.ctl_json(&["key", "down"]);
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "resume", id10])
    );
    assert_shows(&menu.screen(), &["a every directory"]);

    // Every directory keeps the selection, and any session can resume.
    let state = menu.ctl_json(&["key", "a"]);
    assert_eq!(state["sessions"]["scope"], "all");
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "resume", id10])
    );
    let state = menu.select_session("Fix the flaky tests");
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "--profile", "gateway", "resume", id1])
    );
    // The profiles view passes the name to Codex unchanged.
    let state = menu.ctl_json(&["key", "tab"]);
    assert_eq!(
        state["command"],
        serde_json::json!(["codex", "resume", "Review the docs"])
    );
    menu.ctl_json(&["key", "tab", "enter"]);
    assert_eq!(
        menu.finish(),
        format!("codex --profile gateway resume {id1}\n")
    );
}

#[test]
fn ctl_state_lists_only_the_sessions_on_screen() {
    let sessions = SessionsFixture::new();
    let menu = Headless::start_in_project_sized(&sessions, "80x16", &[]);
    menu.ctl_json(&["key", "tab", "a"]);
    let state = menu.sessions_state();
    let listed = &state["sessions"];
    assert_eq!(listed["total"], 9);
    assert_eq!(shown_from(&state), 0);
    let shown = listed["list"].as_array().unwrap().len();
    assert!(0 < shown && shown < 9, "{state}");

    // Moving the selection scrolls the table, and the list with it.
    let state = menu.ctl_json(&["key", "end"]);
    assert_eq!(state["sessions"]["selected_index"], 8);
    let rows = session_rows(&state);
    assert_eq!(shown_from(&state) + rows.len(), 9);
    assert_eq!(rows.last().unwrap(), "Review the docs | gpt-base | default");

    // The profiles view shows no sessions.
    let state = menu.ctl_json(&["key", "tab"]);
    assert_eq!(state["sessions"]["list"], serde_json::json!([]));
    assert_eq!(state["sessions"]["total"], 9);
}
