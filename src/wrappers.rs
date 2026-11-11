//! The launcher's own file, `$CODEX_HOME/codex-launcher.toml`. It names the
//! commands that launch and check profiles, in place of the `codex-<name>` and
//! `codex-<name>.status` executables on `PATH`:
//!
//! ```toml
//! [[wrapper]]
//! profiles = ["work", "work-fast"]
//! launch = ["~/bin/work-codex", "{profile}"]
//! status = ["~/bin/work-codex", "--status", "{profile}"]
//! ```
//!
//! Each command is the program, then its arguments. `{profile}` becomes the
//! profile's name, and a program that starts with `~/` is in the home
//! directory. A problem leaves out only the command or name that has it.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};

use crate::launch::Program;
use crate::profile::DEFAULT_PROFILE;

/// The file's name in the Codex home.
pub const FILE_NAME: &str = "codex-launcher.toml";
const PLACEHOLDER: &str = "{profile}";
const KEYS: &[&str] = &["launch", "profiles", "status"];

/// The commands that the file gives one profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wrapper {
    /// What launching runs instead of `codex --profile <name>`. Codex
    /// arguments follow its own.
    pub launch: Option<Program>,
    /// What server checks run instead of asking the profile's server.
    pub status: Option<Program>,
}

/// Read the file in `codex_home`, if there is one, for the named profiles.
/// Problems become warnings.
pub fn load(
    codex_home: &Path,
    profiles: &[String],
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, Wrapper> {
    let text = match crate::profile::read_limited(&codex_home.join(FILE_NAME)) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            warnings.push(format!("{FILE_NAME}: cannot read file: {error}"));
            return BTreeMap::new();
        }
    };
    match text.parse::<DocumentMut>() {
        Ok(document) => parse(&document, profiles, home, warnings),
        Err(error) => {
            warnings.push(format!("{FILE_NAME}: invalid TOML: {}", error.message()));
            BTreeMap::new()
        }
    }
}

fn parse(
    document: &DocumentMut,
    profiles: &[String],
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, Wrapper> {
    let mut wrappers = BTreeMap::new();
    for (key, _) in document.iter() {
        if key != "wrapper" {
            warnings.push(format!("{FILE_NAME}: unknown key {key:?}"));
        }
    }
    let Some(item) = document.get("wrapper") else {
        return wrappers;
    };
    // [[wrapper]] tables, or the same entries as `wrapper = [{ ... }]`.
    let Ok(entries) = item.clone().into_array_of_tables() else {
        warnings.push(format!(
            "{FILE_NAME}: write each wrapper as a [[wrapper]] table"
        ));
        return wrappers;
    };
    for (index, entry) in entries.iter().enumerate() {
        let mut warn = |problem: String| {
            warnings.push(format!("{FILE_NAME}: wrapper {}: {problem}", index + 1));
        };
        for (key, _) in entry.iter() {
            if !KEYS.contains(&key) {
                warn(format!("unknown key {key:?}"));
            }
        }
        let names = match strings(entry.get("profiles")) {
            Some(names) if !names.is_empty() => names,
            _ => {
                warn("profiles must be a list of profile names".to_owned());
                continue;
            }
        };
        // A command with a problem is left out, but the entry still names
        // its profiles, so a wrapper on PATH cannot take over.
        let mut command = |key: &str| {
            template(entry, key, home).unwrap_or_else(|problem| {
                warn(problem);
                None
            })
        };
        let launch = command("launch");
        let status = command("status");
        if !entry.contains_key("launch") && !entry.contains_key("status") {
            warn("needs a launch command, a status command, or both".to_owned());
        }
        for name in names {
            if name == DEFAULT_PROFILE {
                warn(format!(
                    "{DEFAULT_PROFILE:?} always runs codex with config.toml alone"
                ));
            } else if !profiles.iter().any(|profile| profile == name) {
                warn(format!("no profile is named {name:?}"));
            } else if wrappers.contains_key(name) {
                warn(format!("{name:?} is already named above"));
            } else {
                let wrapper = Wrapper {
                    launch: launch.as_ref().map(|command| command.fill(name)),
                    status: status.as_ref().map(|command| command.fill(name)),
                };
                wrappers.insert(name.to_owned(), wrapper);
            }
        }
    }
    wrappers
}

/// A list of strings, or `None` for anything else.
fn strings(item: Option<&Item>) -> Option<Vec<&str>> {
    item?
        .as_array()?
        .iter()
        .map(|value| value.as_str())
        .collect()
}

/// A command as the file gives it, before `{profile}` becomes a name.
struct Template<'a> {
    /// The home directory for a program that starts with `~/`.
    home: Option<&'a Path>,
    /// The program, without a leading `~/`.
    program: &'a str,
    args: Vec<&'a str>,
}

/// Read the command under `key`, if the entry has one.
fn template<'a>(
    entry: &'a Table,
    key: &str,
    home: Option<&'a Path>,
) -> Result<Option<Template<'a>>, String> {
    let Some(item) = entry.get(key) else {
        return Ok(None);
    };
    let words = strings(Some(item)).unwrap_or_default();
    let Some((&program, args)) = words.split_first() else {
        return Err(format!(
            "{key} must be a list of strings: the program, then its arguments"
        ));
    };
    // A relative program would depend on the directory the launcher runs in.
    let (home, program) = match program.strip_prefix("~/") {
        Some("") => return Err(format!("{key} names no program after ~/")),
        Some(rest) => match home {
            Some(home) => (Some(home), rest),
            None => {
                return Err(format!(
                    "{key} starts with ~/, but HOME is unset or not absolute"
                ));
            }
        },
        None if program.starts_with('/') => (None, program),
        None => {
            return Err(format!(
                "{key} must start with an absolute path or ~/, not {program:?}"
            ));
        }
    };
    Ok(Some(Template {
        home,
        program,
        args: args.to_vec(),
    }))
}

impl Template<'_> {
    fn fill(&self, name: &str) -> Program {
        let program = self.program.replace(PLACEHOLDER, name);
        Program {
            path: match self.home {
                Some(home) => home.join(program),
                None => PathBuf::from(program),
            },
            args: self
                .args
                .iter()
                .map(|arg| arg.replace(PLACEHOLDER, name))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str, home: Option<&Path>) -> (BTreeMap<String, Wrapper>, Vec<String>) {
        let profiles = ["alpha", "beta", "delta", "gamma", "omega"].map(str::to_owned);
        let mut warnings = Vec::new();
        let wrappers = parse(&text.parse().unwrap(), &profiles, home, &mut warnings);
        (wrappers, warnings)
    }

    fn program(path: &str, args: &[&str]) -> Program {
        Program {
            path: PathBuf::from(path),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[test]
    fn entries_give_each_named_profile_its_commands() {
        let (wrappers, warnings) = parsed(
            r#"
[[wrapper]]
profiles = ["alpha", "beta"]
launch = ["~/bin/run", "{profile}", "--name={profile}"]
status = ["/opt/run", "--status", "{profile}"]

[[wrapper]]
profiles = ["gamma"]
status = ["~/bin/check-{profile}"]
"#,
            Some(Path::new("/home/user")),
        );
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            wrappers["alpha"],
            Wrapper {
                launch: Some(program("/home/user/bin/run", &["alpha", "--name=alpha"])),
                status: Some(program("/opt/run", &["--status", "alpha"])),
            }
        );
        assert_eq!(
            wrappers["beta"].launch,
            Some(program("/home/user/bin/run", &["beta", "--name=beta"]))
        );
        assert_eq!(
            wrappers["gamma"],
            Wrapper {
                launch: None,
                status: Some(program("/home/user/bin/check-gamma", &[])),
            }
        );
    }

    #[test]
    fn problems_leave_out_only_the_command_or_name_that_has_them() {
        let (wrappers, warnings) = parsed(
            r#"
note = "hi"

[[wrapper]]
profiles = ["alpha", "default", "missing", "alpha"]
launch = ["/bin/run", "{profile}"]
lauch = ["/bin/typo"]

[[wrapper]]
profiles = ["alpha", "beta"]
status = ["/bin/second"]

[[wrapper]]
profiles = ["gamma"]
launch = ["bin/run"]
status = ["~/check"]

[[wrapper]]
profiles = ["delta"]
launch = []
status = ["~/"]

[[wrapper]]
profiles = "omega"
launch = ["/bin/run"]

[[wrapper]]
profiles = ["omega"]
"#,
            None,
        );
        assert_eq!(
            warnings,
            [
                r#"codex-launcher.toml: unknown key "note""#,
                r#"codex-launcher.toml: wrapper 1: unknown key "lauch""#,
                r#"codex-launcher.toml: wrapper 1: "default" always runs codex with config.toml alone"#,
                r#"codex-launcher.toml: wrapper 1: no profile is named "missing""#,
                r#"codex-launcher.toml: wrapper 1: "alpha" is already named above"#,
                r#"codex-launcher.toml: wrapper 2: "alpha" is already named above"#,
                r#"codex-launcher.toml: wrapper 3: launch must start with an absolute path or ~/, not "bin/run""#,
                "codex-launcher.toml: wrapper 3: status starts with ~/, but HOME is unset or not absolute",
                "codex-launcher.toml: wrapper 4: launch must be a list of strings: the program, then its arguments",
                "codex-launcher.toml: wrapper 4: status names no program after ~/",
                "codex-launcher.toml: wrapper 5: profiles must be a list of profile names",
                "codex-launcher.toml: wrapper 6: needs a launch command, a status command, or both",
            ]
        );
        let none = Wrapper {
            launch: None,
            status: None,
        };
        assert_eq!(
            wrappers,
            BTreeMap::from([
                (
                    "alpha".to_owned(),
                    Wrapper {
                        launch: Some(program("/bin/run", &["alpha"])),
                        status: None,
                    }
                ),
                (
                    "beta".to_owned(),
                    Wrapper {
                        launch: None,
                        status: Some(program("/bin/second", &[])),
                    }
                ),
                // Named with only broken commands, these still ignore PATH.
                ("delta".to_owned(), none.clone()),
                ("gamma".to_owned(), none.clone()),
                ("omega".to_owned(), none),
            ])
        );
    }

    #[test]
    fn inline_tables_work_like_wrapper_tables() {
        let (wrappers, warnings) = parsed(
            r#"wrapper = [{ profiles = ["alpha"], launch = ["/bin/run", "{profile}"] }]"#,
            None,
        );
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            wrappers["alpha"].launch,
            Some(program("/bin/run", &["alpha"]))
        );
    }

    #[test]
    fn a_missing_file_is_fine_and_a_broken_one_is_a_warning() {
        let home = tempfile::tempdir().unwrap();
        let profiles = ["alpha".to_owned()];
        let load_warnings = || {
            let mut warnings = Vec::new();
            assert!(load(home.path(), &profiles, None, &mut warnings).is_empty());
            warnings
        };
        assert!(load_warnings().is_empty());

        std::fs::write(home.path().join(FILE_NAME), "[[wrapper]\n").unwrap();
        let warnings = load_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].starts_with("codex-launcher.toml: invalid TOML: "),
            "{warnings:?}"
        );

        std::fs::write(
            home.path().join(FILE_NAME),
            "[wrapper]\nprofiles = [\"alpha\"]\nlaunch = [\"/bin/run\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_warnings(),
            ["codex-launcher.toml: write each wrapper as a [[wrapper]] table"]
        );
    }
}
