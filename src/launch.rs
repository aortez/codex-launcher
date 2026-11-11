//! Building and running the Codex command for a profile.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use crate::profile::Profile;

/// The Codex executable, resolved through `PATH` when it runs.
pub const CODEX: &str = "codex";

/// The profile name whose wrapper would be this launcher itself.
const SELF_NAME: &str = "launcher";

/// The command that launching a profile runs.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Command {
    /// The program name shown to the user.
    pub program: String,
    pub args: Vec<String>,
    /// The resolved `codex-<profile>` wrapper, when one is on `PATH`.
    pub wrapper: Option<PathBuf>,
    /// The resolved `codex-<profile>.status` command, when one is on `PATH`.
    /// Server checks run it instead of asking the server themselves.
    pub status: Option<PathBuf>,
}

impl Command {
    /// Override effort before subcommands/prompts, removing competing config
    /// flags while leaving literal arguments after `--` alone.
    pub fn with_effort(&self, effort: &str) -> Self {
        let mut command = self.clone();
        command.args = vec![
            "-c".to_owned(),
            format!("model_reasoning_effort={}", toml_edit::Value::from(effort)),
        ];
        let mut index = 0;
        while index < self.args.len() {
            if self.args[index] == "--" {
                command.args.extend_from_slice(&self.args[index..]);
                break;
            }
            if let Some((count, assignment)) = crate::effort::config_argument(&self.args, index)
                && crate::effort::is_effort_assignment(assignment)
            {
                index += count;
                continue;
            }
            command.args.push(self.args[index].clone());
            index += 1;
        }
        command
    }

    /// Build the command for `profile`, with `extra` arguments passed through.
    ///
    /// A profile named `NAME` runs `codex-NAME` instead of `codex --profile NAME`
    /// when that executable is on `path`. The wrapper is expected to select the
    /// profile itself.
    pub fn for_profile(profile: &Profile, extra: &[String], path: Option<&OsStr>) -> Self {
        let (wrapper, status) = if profile.is_default() {
            (None, None)
        } else {
            (
                find_wrapper(&profile.name, path),
                find_executable(&status_name(&profile.name), path),
            )
        };
        let mut args = Vec::new();
        let program = if wrapper.is_some() {
            wrapper_name(&profile.name)
        } else {
            if !profile.is_default() {
                args.extend(profile_args(&profile.name));
            }
            CODEX.to_owned()
        };
        args.extend(extra.iter().cloned());
        Self {
            program,
            args,
            wrapper,
            status,
        }
    }

    /// The full argument vector, program first.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }

    /// The command as a shell-quoted line.
    pub fn display(&self) -> String {
        self.argv()
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Replace this process with the command. Returns only on failure.
    pub fn exec(&self) -> anyhow::Error {
        let program: &OsStr = match &self.wrapper {
            Some(path) => path.as_os_str(),
            None => OsStr::new(&self.program),
        };
        let error = std::process::Command::new(program).args(&self.args).exec();
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("cannot run {}: not found on PATH", self.program)
        } else {
            anyhow::anyhow!("cannot run {}: {error}", self.program)
        }
    }
}

/// The wrapper executable name for a profile.
pub fn wrapper_name(profile: &str) -> String {
    format!("{CODEX}-{profile}")
}

/// The status command's executable name for a profile. Profile names cannot
/// contain a dot, so it never names another profile's wrapper.
pub fn status_name(profile: &str) -> String {
    format!("{}.status", wrapper_name(profile))
}

/// Codex arguments that select a profile.
fn profile_args(name: &str) -> Vec<String> {
    // A value that starts with '-' would be read as a flag after a space.
    if name.starts_with('-') {
        vec![format!("--profile={name}")]
    } else {
        vec!["--profile".to_owned(), name.to_owned()]
    }
}

/// Find an executable `codex-<profile>` on `path`.
fn find_wrapper(profile: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if profile == SELF_NAME {
        return None;
    }
    let name = wrapper_name(profile);
    find_executable(&name, path)
}

pub fn find_executable(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Quote `arg` for a POSIX shell when it needs quoting.
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&byte));
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile;
    use std::fs;

    fn catalog(names: &[&str]) -> (tempfile::TempDir, profile::Catalog) {
        let home = tempfile::tempdir().unwrap();
        for name in names {
            fs::write(
                home.path().join(format!("{name}.config.toml")),
                "model = \"m\"\n",
            )
            .unwrap();
        }
        let catalog = profile::load(home.path()).unwrap();
        (home, catalog)
    }

    fn executable(dir: &Path, name: &str) {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn default_runs_plain_codex_with_extra_args() {
        let (_home, catalog) = catalog(&[]);
        let extra = vec!["resume".to_owned(), "--last".to_owned()];
        let command = Command::for_profile(catalog.find("default").unwrap(), &extra, None);
        assert_eq!(command.argv(), ["codex", "resume", "--last"]);
        assert_eq!(command.wrapper, None);
    }

    #[test]
    fn profile_runs_codex_with_profile_flag() {
        let (_home, catalog) = catalog(&["work", "-odd"]);
        let command = Command::for_profile(catalog.find("work").unwrap(), &[], None);
        assert_eq!(command.display(), "codex --profile work");
        let command = Command::for_profile(catalog.find("-odd").unwrap(), &[], None);
        assert_eq!(command.argv(), ["codex", "--profile=-odd"]);
    }

    #[test]
    fn wrapper_on_path_replaces_profile_flag() {
        let (_home, catalog) = catalog(&["work", "plain", "launcher"]);
        let bin = tempfile::tempdir().unwrap();
        executable(bin.path(), "codex-work");
        executable(bin.path(), "codex-launcher");
        fs::write(bin.path().join("codex-plain"), "not executable").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), bin.path()]).unwrap();
        let extra = vec!["exec".to_owned(), "two words".to_owned()];

        let work = Command::for_profile(catalog.find("work").unwrap(), &extra, Some(&path));
        assert_eq!(work.argv(), ["codex-work", "exec", "two words"]);
        assert_eq!(work.wrapper, Some(bin.path().join("codex-work")));
        assert_eq!(work.display(), "codex-work exec 'two words'");

        let plain = Command::for_profile(catalog.find("plain").unwrap(), &[], Some(&path));
        assert_eq!(plain.argv(), ["codex", "--profile", "plain"]);

        let launcher = Command::for_profile(catalog.find("launcher").unwrap(), &[], Some(&path));
        assert_eq!(launcher.argv(), ["codex", "--profile", "launcher"]);
    }

    #[test]
    fn status_command_comes_from_path_for_named_profiles() {
        let (_home, catalog) = catalog(&["work"]);
        let bin = tempfile::tempdir().unwrap();
        executable(bin.path(), "codex-work.status");
        executable(bin.path(), "codex-default.status");
        let path = Some(bin.path().as_os_str());

        let work = Command::for_profile(catalog.find("work").unwrap(), &[], path);
        assert_eq!(work.status, Some(bin.path().join("codex-work.status")));
        // A status command does not change what launching runs.
        assert_eq!(work.argv(), ["codex", "--profile", "work"]);

        let default = Command::for_profile(catalog.find("default").unwrap(), &[], path);
        assert_eq!(default.status, None);
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("model=x"), "model=x");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b"), "'a b'");
    }

    #[test]
    fn effort_overrides_config_flags_but_preserves_literal_arguments() {
        let (_home, catalog) = catalog(&["work"]);
        let extra = [
            "--config",
            "model_reasoning_effort='low'",
            "resume",
            "--last",
            "--config=model_reasoning_effort=medium",
            "-cmodel_reasoning_effort=high",
            "-c",
            "other=true",
            "--",
            "-c",
            "model_reasoning_effort=none",
        ]
        .map(str::to_owned);
        let command = Command::for_profile(catalog.find("work").unwrap(), &extra, None);
        assert_eq!(
            crate::effort::argument_effort(&command.args).as_deref(),
            Some("high")
        );
        let updated = command.with_effort("xhigh");
        assert_eq!(
            updated.argv(),
            [
                "codex",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "--profile",
                "work",
                "resume",
                "--last",
                "-c",
                "other=true",
                "--",
                "-c",
                "model_reasoning_effort=none"
            ]
        );
        assert_eq!(
            crate::effort::argument_effort(&updated.args).as_deref(),
            Some("xhigh")
        );

        let bin = tempfile::tempdir().unwrap();
        executable(bin.path(), "codex-work");
        let command = Command::for_profile(
            catalog.find("work").unwrap(),
            &["exec".into(), "fix tests".into()],
            Some(bin.path().as_os_str()),
        );
        let updated = command.with_effort("high");
        assert_eq!(
            updated.argv(),
            [
                "codex-work",
                "-c",
                "model_reasoning_effort=\"high\"",
                "exec",
                "fix tests"
            ]
        );
        assert_eq!(updated.wrapper, command.wrapper);
    }
}
