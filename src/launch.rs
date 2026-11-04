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
}

impl Command {
    /// Build the command for `profile`, with `extra` arguments passed through.
    ///
    /// A profile named `NAME` runs `codex-NAME` instead of `codex --profile NAME`
    /// when that executable is on `path`. The wrapper is expected to select the
    /// profile itself.
    pub fn for_profile(profile: &Profile, extra: &[String], path: Option<&OsStr>) -> Self {
        let wrapper = if profile.is_default() {
            None
        } else {
            find_wrapper(&profile.name, path)
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
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(&name))
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
    fn quoting() {
        assert_eq!(shell_quote("model=x"), "model=x");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b"), "'a b'");
    }
}
