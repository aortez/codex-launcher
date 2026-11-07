//! codex-launcher: pick a Codex CLI profile, then launch Codex with it.

mod app;
mod control;
mod effort;
mod launch;
mod ollama;
mod profile;
mod provider;
mod tui;
mod ui;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MenuEntry, ProfileView, unknown_profile};
use crate::control::Request;
use crate::launch::Command;
use crate::profile::Catalog;

const ABOUT: &str = "Pick a Codex CLI profile from a menu or by name, then launch Codex with it.";

const AFTER_HELP: &str = "\
Profiles are $CODEX_HOME/<name>.config.toml files (CODEX_HOME defaults to
~/.codex). \"default\" runs Codex with config.toml alone. When an executable
named codex-<name> is on PATH, launching <name> runs it instead of
`codex --profile <name>`; the wrapper must select the profile itself.

Examples:
  codex-launcher                      Open the menu.
  codex-launcher -- resume --last     Open the menu; pass the args to Codex.
  codex-launcher work exec 'hi'       Launch the work profile directly.
  codex-launcher list
  codex-launcher ctl state            Read a running menu's state as JSON.";

#[derive(Parser)]
#[command(
    name = "codex-launcher",
    version,
    about = ABOUT,
    after_help = AFTER_HELP,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Launch this profile without opening the menu.
    #[arg(value_name = "PROFILE")]
    profile: Option<String>,

    /// Arguments passed to Codex. Put them after `--` when there is no profile.
    #[arg(
        value_name = "CODEX_ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    codex_args: Vec<String>,

    /// Run the menu offscreen and drive it only through `ctl`.
    ///
    /// Launching prints the command instead of running it.
    #[arg(long)]
    headless: bool,

    /// Offscreen screen size for `--headless`, as `COLUMNSxROWS`.
    #[arg(
        long,
        value_name = "COLUMNSxROWS",
        default_value = "100x30",
        value_parser = parse_size,
        requires = "headless"
    )]
    size: (u16, u16),

    /// Print the Codex command instead of running it.
    #[arg(long, requires = "profile")]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// List profiles with their effective model, provider, and reasoning effort.
    List {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show one profile's effective settings and its file, with secrets hidden.
    Show {
        profile: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Read or drive a running menu.
    Ctl {
        /// The launcher to control. Needed when more than one is running.
        #[arg(long, global = true)]
        pid: Option<u32>,
        #[command(subcommand)]
        command: CtlCmd,
    },
}

#[derive(Subcommand)]
enum CtlCmd {
    /// List running launchers.
    Instances,
    /// Print the menu state as JSON.
    State,
    /// Print the current screen as plain text.
    Screen,
    /// Press keys in order, then print the new state.
    ///
    /// Names: up, down, left, right, home, end, pgup, pgdn, enter, esc, tab,
    /// backspace, space, ctrl-<letter>, or one character such as q or G.
    Key {
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Move the selection to a profile by name, then print the new state.
    Select { profile: String },
    /// Set a temporary effort, or use "profile" to clear the override.
    Effort { level: String },
    /// Save the selected profile's temporary effort to its config file.
    SaveEffort,
    /// Launch the selected profile, like pressing Enter.
    Launch,
    /// Close the menu without launching.
    Quit,
}

fn parse_size(text: &str) -> Result<(u16, u16), String> {
    let (columns, rows) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected COLUMNSxROWS, such as 100x30, not {text:?}"))?;
    let parse = |value: &str| {
        value
            .parse::<u16>()
            .ok()
            .filter(|value| (1..=1000).contains(value))
            .ok_or_else(|| format!("{value:?} is not a size from 1 to 1000"))
    };
    Ok((parse(columns)?, parse(rows)?))
}

/// Parse arguments, keeping `--` from filling the profile slot.
///
/// clap fills positionals in order even after `--`, so
/// `codex-launcher -- resume` would read `resume` as the profile. When `--`
/// comes before any positional argument, everything after it is for Codex.
fn parse_cli<I>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator,
    I::Item: Into<OsString>,
{
    let mut args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let mut passthrough = Vec::new();
    if let Some(split) = leading_separator(&args) {
        passthrough = args.split_off(split + 1);
        args.pop();
    }
    let mut cli = Cli::try_parse_from(args)?;
    for arg in passthrough {
        let arg = arg.into_string().map_err(|arg| {
            Cli::command().error(
                clap::error::ErrorKind::InvalidUtf8,
                format!("argument is not valid UTF-8: {}", arg.to_string_lossy()),
            )
        })?;
        cli.codex_args.push(arg);
    }
    Ok(cli)
}

/// The index of a `--` that comes before any positional argument.
///
/// Only the menu's own flags can precede it. `--size` takes a value, which may
/// be given as the next argument.
fn leading_separator(args: &[OsString]) -> Option<usize> {
    let mut index = 1;
    while let Some(arg) = args.get(index) {
        match arg.to_str() {
            Some("--") => return Some(index),
            Some("--size") => index += 2,
            Some(flag) if flag.starts_with('-') => index += 1,
            _ => return None,
        }
    }
    None
}

fn main() -> ExitCode {
    let cli = match parse_cli(std::env::args_os()) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("codex-launcher: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Some(Cmd::List { json }) => list(&load()?, json),
        Some(Cmd::Show { profile, json }) => show(&load()?, &profile, json),
        Some(Cmd::Ctl { pid, command }) => ctl(pid, command),
        None => menu_or_launch(cli),
    }
}

fn load() -> Result<Catalog> {
    profile::load(&profile::codex_home()?)
}

fn path_var() -> Option<OsString> {
    std::env::var_os("PATH")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn print_warnings(catalog: &Catalog) {
    for warning in &catalog.warnings {
        eprintln!("codex-launcher: warning: {warning}");
    }
}

fn menu_or_launch(cli: Cli) -> Result<ExitCode> {
    let catalog = load()?;
    let path = path_var();
    if let Some(name) = &cli.profile {
        if cli.headless {
            bail!("--headless opens the menu; leave out the profile name");
        }
        let profile = catalog
            .find(name)
            .ok_or_else(|| anyhow::anyhow!(unknown_profile(&catalog, name)))?;
        let command = Command::for_profile(profile, &cli.codex_args, path.as_deref());
        if cli.dry_run {
            println!("{}", command.display());
            return Ok(ExitCode::SUCCESS);
        }
        return Err(command.exec());
    }

    let app = App::new(catalog, &cli.codex_args, path.as_deref(), home_dir());
    print_warnings(app.catalog());
    if cli.headless {
        let (columns, rows) = cli.size;
        return tui::run_headless(app, columns, rows);
    }
    tui::run_terminal(app)
}

fn list(catalog: &Catalog, json: bool) -> Result<ExitCode> {
    print_warnings(catalog);
    let path = path_var();
    if json {
        let views = catalog
            .profiles
            .iter()
            .map(|profile| {
                let command = Command::for_profile(profile, &[], path.as_deref());
                serde_json::to_value(ProfileView::new(profile, &command, false))
            })
            .collect::<Result<Vec<_>, _>>()?;
        print_json(&Value::Array(views))?;
        return Ok(ExitCode::SUCCESS);
    }

    let header = ["PROFILE", "MODEL", "PROVIDER", "EFFORT", "DESCRIPTION"].map(String::from);
    let rows: Vec<[String; 5]> = catalog
        .profiles
        .iter()
        .map(|profile| {
            let entry = MenuEntry::new(profile);
            let mut provider = entry.provider.to_owned();
            if profile.provider.missing {
                provider.push_str(" (not defined)");
            }
            [
                entry.name.to_owned(),
                entry.model.unwrap_or("-").to_owned(),
                provider,
                entry.effort.unwrap_or("-").to_owned(),
                profile.description.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let mut widths = [0usize; 4];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.width());
        }
    }
    let mut out = io::stdout().lock();
    for row in std::iter::once(&header).chain(&rows) {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(widths) {
            line.push_str(cell);
            line.push_str(&" ".repeat(width - cell.width() + 2));
        }
        line.push_str(&row[4]);
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(ExitCode::SUCCESS)
}

fn show(catalog: &Catalog, name: &str, json: bool) -> Result<ExitCode> {
    let profile = catalog
        .find(name)
        .ok_or_else(|| anyhow::anyhow!(unknown_profile(catalog, name)))?;
    let path = path_var();
    if json {
        let command = Command::for_profile(profile, &[], path.as_deref());
        print_json(&serde_json::to_value(ProfileView::new(
            profile, &command, true,
        ))?)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut app = App::new(catalog.clone(), &[], path.as_deref(), home_dir());
    app.select_name(name).map_err(anyhow::Error::msg)?;
    let mut out = io::stdout().lock();
    for line in ui::details(&app).lines {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        writeln!(out, "{}", text.trim_end())?;
    }
    Ok(ExitCode::SUCCESS)
}

fn ctl(pid: Option<u32>, command: CtlCmd) -> Result<ExitCode> {
    let dir = control::runtime_dir();
    let request = match command {
        CtlCmd::Instances => return instances(&dir),
        CtlCmd::State => Request::State,
        CtlCmd::Screen => Request::Screen,
        CtlCmd::Key { keys } => Request::Key { keys },
        CtlCmd::Select { profile } => Request::Select { profile },
        CtlCmd::Effort { level } => Request::Effort { level },
        CtlCmd::SaveEffort => Request::SaveEffort,
        CtlCmd::Launch => Request::Launch,
        CtlCmd::Quit => Request::Quit,
    };
    let endpoint = control::pick(&dir, pid)?;
    let response = control::send(&endpoint.path, &request)?;
    if response["ok"] != Value::Bool(true) {
        bail!(
            "{}",
            response["error"].as_str().unwrap_or("the request failed")
        );
    }
    if request == Request::Screen {
        let screen = response["screen"]
            .as_str()
            .context("the reply has no screen")?;
        print!("{screen}");
    } else {
        print_json(&response["state"])?;
    }
    Ok(ExitCode::SUCCESS)
}

fn instances(dir: &Path) -> Result<ExitCode> {
    let mut found = Vec::new();
    for endpoint in control::endpoints(dir)? {
        let socket = endpoint.path.display().to_string();
        found.push(match control::send(&endpoint.path, &Request::State) {
            Ok(response) if response["ok"] == Value::Bool(true) => {
                let state = &response["state"];
                json!({
                    "pid": endpoint.pid,
                    "mode": state["mode"],
                    "tty": state["tty"],
                    "cwd": state["cwd"],
                    "status": state["status"],
                    "selected": state["selected"],
                    "socket": socket,
                })
            }
            Ok(response) => {
                json!({ "pid": endpoint.pid, "socket": socket, "error": response["error"] })
            }
            Err(error) => {
                json!({ "pid": endpoint.pid, "socket": socket, "error": format!("{error:#}") })
            }
        });
    }
    print_json(&Value::Array(found))?;
    Ok(ExitCode::SUCCESS)
}

fn print_json(value: &Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod test_support {
    use std::path::Path;

    use crate::app::App;
    use crate::profile;

    /// A copy of the fake Codex home in tests/fixtures.
    pub fn fixture_home() -> tempfile::TempDir {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-home");
        let home = tempfile::tempdir().unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), home.path().join(entry.file_name())).unwrap();
        }
        home
    }

    /// A menu over the fixture home, with no wrappers on the search path.
    pub fn fixture_app() -> (tempfile::TempDir, App) {
        let home = fixture_home();
        let catalog = profile::load(home.path()).unwrap();
        let app = App::new(catalog, &[], None, None);
        (home, app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        parse_cli(std::iter::once("codex-launcher").chain(args.iter().copied()))
    }

    #[test]
    fn parses_profile_and_passthrough_args() {
        let cli = parse(&["work", "exec", "--json", "hi"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
        assert_eq!(cli.codex_args, ["exec", "--json", "hi"]);

        let cli = parse(&["--", "resume", "--last"]).unwrap();
        assert_eq!(cli.profile, None);
        assert_eq!(cli.codex_args, ["resume", "--last"]);

        let cli = parse(&["--headless", "--size", "80x24", "--", "resume"]).unwrap();
        assert!(cli.headless);
        assert_eq!(cli.size, (80, 24));
        assert_eq!(cli.profile, None);
        assert_eq!(cli.codex_args, ["resume"]);

        let cli = parse(&["--", "--", "x"]).unwrap();
        assert_eq!(cli.codex_args, ["--", "x"]);

        // After a profile, `--` is clap's: it ends launcher options.
        let cli = parse(&["work", "--", "--headless"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
        assert!(!cli.headless);
        assert_eq!(cli.codex_args, ["--headless"]);

        let cli = parse(&["list"]).unwrap();
        assert!(matches!(cli.command, Some(Cmd::List { json: false })));

        let cli = parse(&["ctl", "key", "down", "enter"]).unwrap();
        assert!(matches!(cli.command, Some(Cmd::Ctl { .. })));

        assert!(parse(&["--dry-run"]).is_err());
        assert!(parse(&["--size", "80x24"]).is_err());
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("100x30"), Ok((100, 30)));
        assert_eq!(parse_size("80X24"), Ok((80, 24)));
        assert!(parse_size("100").is_err());
        assert!(parse_size("0x5").is_err());
        assert!(parse_size("5x2000").is_err());
    }
}
