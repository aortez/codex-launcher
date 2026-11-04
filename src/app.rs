//! Launcher state, keys, and the actions that change it.
//!
//! The terminal and the control socket share this state and keymap, so a
//! `ctl key` request behaves exactly like the same key typed in the menu.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use serde::Serialize;

use crate::launch::Command;
use crate::profile::{Catalog, Profile, Provider, Setting};

/// A key press, from the terminal or from `ctl key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Esc,
    Tab,
    Backspace,
    Char(char),
    Ctrl(char),
}

const NAMED_KEYS: &[(&str, Key)] = &[
    ("up", Key::Up),
    ("down", Key::Down),
    ("left", Key::Left),
    ("right", Key::Right),
    ("home", Key::Home),
    ("end", Key::End),
    ("pgup", Key::PageUp),
    ("pageup", Key::PageUp),
    ("pgdn", Key::PageDown),
    ("pagedown", Key::PageDown),
    ("enter", Key::Enter),
    ("return", Key::Enter),
    ("esc", Key::Esc),
    ("escape", Key::Esc),
    ("tab", Key::Tab),
    ("backspace", Key::Backspace),
    ("space", Key::Char(' ')),
];

impl Key {
    /// Parse a key name such as `down`, `enter`, `q`, or `ctrl-c`.
    ///
    /// A single character is that character, so `G` and `g` differ. Longer
    /// names ignore case.
    pub fn parse(name: &str) -> Result<Self, String> {
        if let Some(c) = single_char(name) {
            return Ok(Self::Char(c));
        }
        let lower = name.to_ascii_lowercase();
        let ctrl = lower
            .strip_prefix("ctrl-")
            .or_else(|| lower.strip_prefix("ctrl+"));
        if let Some(rest) = ctrl {
            return single_char(rest)
                .filter(char::is_ascii_alphabetic)
                .map(Self::Ctrl)
                .ok_or_else(|| format!("unknown key {name:?}; use ctrl-<letter>, such as ctrl-c"));
        }
        NAMED_KEYS
            .iter()
            .find(|(key_name, _)| *key_name == lower)
            .map(|(_, key)| *key)
            .ok_or_else(|| {
                format!(
                    "unknown key {name:?}; use one character or one of: up, down, left, right, \
                     home, end, pgup, pgdn, enter, esc, tab, backspace, space, ctrl-<letter>"
                )
            })
    }

    /// Convert a terminal key event, ignoring releases and unmapped keys.
    pub fn from_event(event: &KeyEvent) -> Option<Self> {
        if event.kind == KeyEventKind::Release {
            return None;
        }
        if event.modifiers.contains(KeyModifiers::CONTROL) {
            return match event.code {
                KeyCode::Char(c) if c.is_ascii_alphabetic() => {
                    Some(Self::Ctrl(c.to_ascii_lowercase()))
                }
                _ => None,
            };
        }
        Some(match event.code {
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::Enter => Self::Enter,
            KeyCode::Esc => Self::Esc,
            KeyCode::Tab => Self::Tab,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Char(c) => Self::Char(c),
            _ => return None,
        })
    }
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Something the user can do in the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Previous,
    Next,
    First,
    Last,
    ScrollUp,
    ScrollDown,
    Launch,
    Quit,
}

/// The keymap shared by the terminal and `ctl key`.
pub fn action_for(key: Key) -> Option<Action> {
    Some(match key {
        Key::Up | Key::Char('k') => Action::Previous,
        Key::Down | Key::Char('j') => Action::Next,
        Key::Home | Key::Char('g') => Action::First,
        Key::End | Key::Char('G') => Action::Last,
        Key::PageUp => Action::ScrollUp,
        Key::PageDown => Action::ScrollDown,
        Key::Enter => Action::Launch,
        Key::Esc | Key::Char('q') | Key::Ctrl('c') => Action::Quit,
        _ => return None,
    })
}

/// Whether the menu is still open, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Selecting,
    Launching(Command),
    Quitting,
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Selecting => "selecting",
            Self::Launching(_) => "launching",
            Self::Quitting => "quitting",
        }
    }
}

/// View state that the renderer updates after each draw.
#[derive(Debug, Default)]
pub struct View {
    pub list: ListState,
    /// The details pane scroll offset, in wrapped lines.
    pub details_scroll: u16,
    pub details_max_scroll: u16,
    /// The number of visible lines in the details pane.
    pub details_page: u16,
}

/// The launcher menu state.
pub struct App {
    catalog: Catalog,
    commands: Vec<Command>,
    selected: usize,
    status: Status,
    home: Option<PathBuf>,
    /// A one-line warning shown in the footer.
    pub notice: Option<String>,
    pub view: View,
}

impl App {
    /// Create the menu with the first entry selected.
    ///
    /// `extra_args` are appended to every launch command, and `path` is the
    /// search path for `codex-<profile>` wrappers.
    pub fn new(
        catalog: Catalog,
        extra_args: &[String],
        path: Option<&OsStr>,
        home: Option<PathBuf>,
    ) -> Self {
        let commands = catalog
            .profiles
            .iter()
            .map(|profile| Command::for_profile(profile, extra_args, path))
            .collect();
        Self {
            catalog,
            commands,
            selected: 0,
            status: Status::Selecting,
            home,
            notice: None,
            view: View::default(),
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_profile(&self) -> &Profile {
        &self.catalog.profiles[self.selected]
    }

    /// The command that launching the selected profile runs.
    pub fn command(&self) -> &Command {
        &self.commands[self.selected]
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The user's home directory, for shortening displayed paths.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub fn is_done(&self) -> bool {
        self.status != Status::Selecting
    }

    /// Apply the action that `key` maps to, if any.
    pub fn press(&mut self, key: Key) {
        if let Some(action) = action_for(key) {
            self.apply(action);
        }
    }

    pub fn apply(&mut self, action: Action) {
        if self.is_done() {
            return;
        }
        let last = self.catalog.profiles.len() - 1;
        match action {
            Action::Previous => self.select(self.selected.saturating_sub(1)),
            Action::Next => self.select((self.selected + 1).min(last)),
            Action::First => self.select(0),
            Action::Last => self.select(last),
            Action::ScrollUp => {
                self.view.details_scroll =
                    self.view.details_scroll.saturating_sub(self.page_step());
            }
            Action::ScrollDown => {
                self.view.details_scroll = self
                    .view
                    .details_scroll
                    .saturating_add(self.page_step())
                    .min(self.view.details_max_scroll);
            }
            Action::Launch => self.status = Status::Launching(self.command().clone()),
            Action::Quit => self.status = Status::Quitting,
        }
    }

    /// Move the selection to the profile called `name`.
    pub fn select_name(&mut self, name: &str) -> Result<(), String> {
        if self.is_done() {
            return Err(format!("the launcher is already {}", self.status.name()));
        }
        let index = self
            .catalog
            .index_of(name)
            .ok_or_else(|| unknown_profile(&self.catalog, name))?;
        self.select(index);
        Ok(())
    }

    /// One page of details scrolling, keeping one line of context.
    fn page_step(&self) -> u16 {
        self.view.details_page.saturating_sub(1).max(1)
    }

    fn select(&mut self, index: usize) {
        if index != self.selected {
            self.selected = index;
            self.view.details_scroll = 0;
        }
    }

    /// The state that `ctl` reports.
    pub fn state<'a>(&'a self, instance: &'a Instance) -> StateView<'a> {
        let command = self.command();
        StateView {
            pid: instance.pid,
            mode: instance.mode,
            tty: instance.tty.as_deref(),
            cwd: instance.cwd.as_deref(),
            codex_home: self.catalog.codex_home.display().to_string(),
            status: self.status.name(),
            selected: &self.selected_profile().name,
            selected_index: self.selected,
            command: command.argv(),
            command_line: command.display(),
            menu: self.catalog.profiles.iter().map(MenuEntry::new).collect(),
            profile: ProfileView::new(self.selected_profile(), command, false),
            details: Details {
                scroll: self.view.details_scroll,
                max_scroll: self.view.details_max_scroll,
            },
            warnings: &self.catalog.warnings,
        }
    }
}

/// The error for a profile name that is not in the catalog.
pub fn unknown_profile(catalog: &Catalog, name: &str) -> String {
    let names: Vec<_> = catalog.profiles.iter().map(|p| p.name.as_str()).collect();
    format!("no profile named {name:?}; profiles: {}", names.join(", "))
}

/// Facts about this launcher process that `ctl instances` shows.
#[derive(Clone, Debug)]
pub struct Instance {
    pub pid: u32,
    pub mode: &'static str,
    pub tty: Option<String>,
    pub cwd: Option<String>,
}

impl Instance {
    pub fn current(mode: &'static str, with_tty: bool) -> Self {
        let tty = with_tty
            .then(|| std::fs::read_link("/proc/self/fd/0").ok())
            .flatten()
            .map(|path| path.display().to_string());
        Self {
            pid: std::process::id(),
            mode,
            tty,
            cwd: std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string()),
        }
    }
}

/// The JSON state that `ctl state` prints.
#[derive(Serialize)]
pub struct StateView<'a> {
    pub pid: u32,
    pub mode: &'a str,
    pub tty: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub codex_home: String,
    pub status: &'static str,
    pub selected: &'a str,
    pub selected_index: usize,
    /// What Enter runs, or what is being launched.
    pub command: Vec<String>,
    pub command_line: String,
    pub menu: Vec<MenuEntry<'a>>,
    pub profile: ProfileView<'a>,
    pub details: Details,
    pub warnings: &'a [String],
}

#[derive(Serialize)]
pub struct Details {
    pub scroll: u16,
    pub max_scroll: u16,
}

/// One menu row.
#[derive(Serialize)]
pub struct MenuEntry<'a> {
    pub name: &'a str,
    pub model: Option<&'a str>,
    pub provider: &'a str,
    pub effort: Option<&'a str>,
}

impl<'a> MenuEntry<'a> {
    pub fn new(profile: &'a Profile) -> Self {
        Self {
            name: &profile.name,
            model: profile.model_name(),
            provider: &profile.provider.id,
            effort: profile
                .effort
                .as_ref()
                .map(|setting| setting.value.as_str()),
        }
    }
}

/// A profile's settings, as `show --json`, `list --json`, and `ctl state` report them.
#[derive(Serialize)]
pub struct ProfileView<'a> {
    pub name: &'a str,
    pub path: String,
    pub description: Option<&'a str>,
    pub model: Option<&'a Setting>,
    pub provider: &'a Provider,
    pub effort: Option<&'a Setting>,
    pub context_window: Option<&'a Setting>,
    pub sandbox: Option<&'a Setting>,
    pub approval: Option<&'a Setting>,
    pub command: Vec<String>,
    pub command_line: String,
    pub wrapper: Option<String>,
    pub error: Option<&'a str>,
    /// The redacted file text, included only on request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<&'a str>,
}

impl<'a> ProfileView<'a> {
    pub fn new(profile: &'a Profile, command: &Command, with_file: bool) -> Self {
        Self {
            name: &profile.name,
            path: profile.path.display().to_string(),
            description: profile.description.as_deref(),
            model: profile.model.as_ref(),
            provider: &profile.provider,
            effort: profile.effort.as_ref(),
            context_window: profile.context_window.as_ref(),
            sandbox: profile.sandbox.as_ref(),
            approval: profile.approval.as_ref(),
            command: command.argv(),
            command_line: command.display(),
            wrapper: command
                .wrapper
                .as_ref()
                .map(|path| path.display().to_string()),
            error: profile.error.as_deref(),
            file: if with_file {
                profile.raw.as_deref()
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture_app;

    #[test]
    fn parses_key_names() {
        assert_eq!(Key::parse("down"), Ok(Key::Down));
        assert_eq!(Key::parse("PgDn"), Ok(Key::PageDown));
        assert_eq!(Key::parse("G"), Ok(Key::Char('G')));
        assert_eq!(Key::parse("g"), Ok(Key::Char('g')));
        assert_eq!(Key::parse("ctrl-c"), Ok(Key::Ctrl('c')));
        assert_eq!(Key::parse("Ctrl+C"), Ok(Key::Ctrl('c')));
        assert_eq!(Key::parse("space"), Ok(Key::Char(' ')));
        assert!(Key::parse("dwn").is_err());
        assert!(Key::parse("ctrl-1").is_err());
        assert!(Key::parse("").is_err());
    }

    #[test]
    fn converts_terminal_events() {
        let press = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            Key::from_event(&press(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            Some(Key::Ctrl('c'))
        );
        assert_eq!(
            Key::from_event(&press(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some(Key::Char('G'))
        );
        let mut release = press(KeyCode::Down, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(Key::from_event(&release), None);
    }

    #[test]
    fn navigation_stays_in_bounds() {
        let (_home, mut app) = fixture_app();
        let names: Vec<_> = app
            .catalog()
            .profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(names, ["default", "gateway", "local"]);
        app.press(Key::Up);
        assert_eq!(app.selected(), 0);
        app.press(Key::Down);
        app.press(Key::Char('j'));
        app.press(Key::Down);
        assert_eq!(app.selected(), 2);
        app.press(Key::Char('g'));
        assert_eq!(app.selected(), 0);
        app.press(Key::End);
        assert_eq!(app.selected(), 2);
        app.press(Key::Char('x'));
        assert_eq!(app.selected(), 2);
        assert_eq!(app.status(), &Status::Selecting);
    }

    #[test]
    fn selecting_resets_details_scroll() {
        let (_home, mut app) = fixture_app();
        app.view.details_max_scroll = 10;
        app.view.details_page = 4;
        app.press(Key::PageDown);
        assert_eq!(app.view.details_scroll, 3);
        app.press(Key::PageDown);
        app.press(Key::PageDown);
        app.press(Key::PageDown);
        assert_eq!(app.view.details_scroll, 10);
        app.press(Key::PageUp);
        assert_eq!(app.view.details_scroll, 7);
        app.press(Key::Down);
        assert_eq!(app.view.details_scroll, 0);
    }

    #[test]
    fn select_by_name() {
        let (_home, mut app) = fixture_app();
        app.select_name("local").unwrap();
        assert_eq!(app.selected_profile().name, "local");
        let error = app.select_name("nope").unwrap_err();
        assert!(error.contains("default, gateway, local"), "{error}");
    }

    #[test]
    fn enter_launches_and_later_keys_are_ignored() {
        let (_home, mut app) = fixture_app();
        app.press(Key::End);
        app.press(Key::Enter);
        let Status::Launching(command) = app.status().clone() else {
            panic!("expected launch, got {:?}", app.status());
        };
        assert_eq!(command.argv(), ["codex", "--profile", "local"]);
        app.press(Key::Up);
        assert_eq!(app.selected(), 2);
        assert!(app.select_name("default").is_err());
    }

    #[test]
    fn quit_keys() {
        for key in [Key::Char('q'), Key::Esc, Key::Ctrl('c')] {
            let (_home, mut app) = fixture_app();
            app.press(key);
            assert_eq!(app.status(), &Status::Quitting, "{key:?}");
        }
    }

    #[test]
    fn state_reports_menu_selection_and_command() {
        let (_home, mut app) = fixture_app();
        app.press(Key::Down);
        let instance = Instance {
            pid: 42,
            mode: "headless",
            tty: None,
            cwd: Some("/work".into()),
        };
        let state = serde_json::to_value(app.state(&instance)).unwrap();
        assert_eq!(state["pid"], 42);
        assert_eq!(state["status"], "selecting");
        assert_eq!(state["selected"], "gateway");
        assert_eq!(state["selected_index"], 1);
        assert_eq!(state["menu"][2]["name"], "local");
        assert_eq!(state["menu"][2]["model"], "qwen3-coder:30b");
        assert_eq!(state["menu"][0]["effort"], "high");
        assert_eq!(state["profile"]["provider"]["id"], "gateway");
        assert_eq!(state["profile"]["model"]["source"], "profile");
        assert_eq!(state["profile"]["effort"]["source"], "base");
        assert_eq!(state["command"][0], "codex");
        assert!(state["profile"].get("file").is_none());
    }
}
