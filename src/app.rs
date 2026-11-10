//! Launcher state, keys, and the actions that change it.
//!
//! The terminal and the control socket share this state and keymap, so a
//! `ctl key` request behaves exactly like the same key typed in the menu.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use serde::Serialize;

use crate::check::{self, Checks, State};
use crate::effort::{self, Choices};
use crate::launch::Command;
use crate::profile::{self, Catalog, Profile, Provider, Setting, Source};

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
    EditEffort,
    SaveEffort,
    Recheck,
    Help,
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
        Key::Char('e') => Action::EditEffort,
        Key::Char('s') => Action::SaveEffort,
        Key::Char('r') => Action::Recheck,
        Key::Char('?') => Action::Help,
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
    pub details: Scroll,
}

/// A scrolling pane's position, which each draw fits to the pane.
#[derive(Debug, Default, Serialize)]
pub struct Scroll {
    /// The first visible line, counting wrapped lines.
    #[serde(rename = "scroll")]
    pub offset: u16,
    /// The offset that shows the last line at the bottom.
    #[serde(rename = "max_scroll")]
    pub max_offset: u16,
    /// The number of visible lines.
    #[serde(skip)]
    pub page: u16,
}

impl Scroll {
    /// Record that the pane shows `page` of the text's `lines`, and keep the
    /// offset within them.
    pub fn fit(&mut self, page: u16, lines: u16) {
        self.page = page;
        self.max_offset = lines.saturating_sub(page);
        self.offset = self.offset.min(self.max_offset);
    }

    fn up(&mut self, lines: u16) {
        self.offset = self.offset.saturating_sub(lines);
    }

    fn down(&mut self, lines: u16) {
        self.offset = self.offset.saturating_add(lines).min(self.max_offset);
    }

    /// One page of scrolling, keeping one line of context.
    fn page_step(&self) -> u16 {
        self.page.saturating_sub(1).max(1)
    }
}

/// The launcher menu state.
pub struct App {
    catalog: Catalog,
    base_commands: Vec<Command>,
    commands: Vec<Command>,
    effort_overrides: Vec<Option<Setting>>,
    argument_effort: Option<Setting>,
    effort_choices: Vec<Choices>,
    effort_queried: Vec<bool>,
    discovery_deadline: Option<Instant>,
    checks: Checks,
    codex_path: Option<PathBuf>,
    extra_args: Vec<String>,
    pub effort_picker: Option<EffortPicker>,
    /// The help screen's position while it is open. The renderer fits it to
    /// the screen, as it does `view`.
    pub help: Option<Scroll>,
    selected: usize,
    status: Status,
    home: Option<PathBuf>,
    /// A one-line warning shown in the footer.
    pub notice: Option<String>,
    pub view: View,
}

#[derive(Debug, Serialize)]
pub struct EffortPicker {
    /// Zero is inheritance; subsequent entries index the available levels.
    pub selected: usize,
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
        let commands: Vec<_> = catalog
            .profiles
            .iter()
            .map(|profile| Command::for_profile(profile, extra_args, path))
            .collect();
        let effort_choices = catalog
            .profiles
            .iter()
            .map(|profile| Choices::load(profile, &catalog.codex_home, extra_args))
            .collect();
        let effort_overrides = vec![None; catalog.profiles.len()];
        let effort_queried = vec![false; catalog.profiles.len()];
        let checks = Checks::new(catalog.profiles.len());
        Self {
            catalog,
            base_commands: commands.clone(),
            commands,
            effort_overrides,
            argument_effort: effort::argument_effort(extra_args).map(|value| Setting {
                value,
                source: Source::Arguments,
            }),
            effort_choices,
            effort_queried,
            discovery_deadline: None,
            checks,
            codex_path: crate::launch::find_executable(crate::launch::CODEX, path),
            extra_args: extra_args.to_vec(),
            effort_picker: None,
            help: None,
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

    pub fn effective_effort(&self) -> Option<&Setting> {
        self.effort_at(self.selected)
    }

    fn effort_at(&self, index: usize) -> Option<&Setting> {
        self.effort_overrides[index]
            .as_ref()
            .or(self.argument_effort.as_ref())
            .or(self.catalog.profiles[index].effort.as_ref())
    }

    pub fn effort_choices(&self) -> &Choices {
        &self.effort_choices[self.selected]
    }

    pub fn has_effort_override(&self) -> bool {
        self.effort_overrides[self.selected].is_some()
    }

    fn discover_effort(&mut self) {
        if self.effort_queried[self.selected] {
            return;
        }
        let timeout = self
            .discovery_deadline
            .map_or(effort::QUERY_TIMEOUT, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(effort::QUERY_TIMEOUT)
            });
        if timeout.is_zero() {
            return;
        }
        let highlighted = self
            .effort_picker
            .as_ref()
            .and_then(|picker| picker.selected.checked_sub(1))
            .and_then(|index| self.effort_choices().levels.get(index))
            .cloned();
        let completed = self.effort_choices[self.selected].discover(
            &self.catalog.profiles[self.selected],
            &self.catalog.codex_home,
            &self.extra_args,
            self.codex_path.as_deref(),
            timeout,
        );
        // A request may give this profile less than its normal query
        // budget. Keep an incomplete lookup eligible for the next action.
        self.effort_queried[self.selected] = completed;
        // A timed-out picker may still be open when an Enter key retries
        // discovery. Preserve its choice by value as the list changes.
        if let Some(picker) = &mut self.effort_picker {
            picker.selected = highlighted
                .and_then(|value| {
                    self.effort_choices[self.selected]
                        .levels
                        .iter()
                        .position(|level| level == &value)
                })
                .map_or(0, |index| index + 1);
        }
    }

    /// Bound all discovery work in one control request, including batched keys.
    pub fn set_discovery_deadline(&mut self, deadline: Option<Instant>) {
        self.discovery_deadline = deadline;
    }

    /// Check every profile's model server, unless a check is still running.
    pub fn check_servers(&mut self) {
        if !self.checks.is_running() {
            self.checks
                .start(&self.catalog, &self.base_commands, &self.extra_args);
        }
    }

    /// Apply finished server checks. Returns whether the screen changed.
    pub fn poll_checks(&mut self) -> bool {
        self.checks.poll()
    }

    /// Whether server checks have started, so the menu shows their marks.
    pub fn checks_started(&self) -> bool {
        self.checks.started()
    }

    pub fn server_status(&self) -> Option<&check::Status> {
        self.server_status_at(self.selected)
    }

    pub fn server_status_at(&self, index: usize) -> Option<&check::Status> {
        self.checks.status(index)
    }

    pub fn inherit_label(&self) -> String {
        let setting = self
            .argument_effort
            .as_ref()
            .or(self.selected_profile().effort.as_ref());
        let label = if self.argument_effort.is_some() {
            "Use launch arguments"
        } else {
            "Use profile setting"
        };
        format!(
            "{label} ({})",
            setting.map_or("Codex default", |s| s.value.as_str())
        )
    }

    /// Set an explicit, validated override, or clear it with `profile`.
    pub fn set_effort(&mut self, level: &str) -> Result<(), String> {
        if self.is_done() {
            return Err(format!("the launcher is already {}", self.status.name()));
        }
        let setting = if level == "profile" {
            None
        } else {
            self.discover_effort();
            if !self.effort_choices().levels.iter().any(|s| s == level) {
                return Err(format!(
                    "effort {level:?} is not available; choose profile{}",
                    self.effort_choices()
                        .levels
                        .iter()
                        .map(|s| format!(", {s}"))
                        .collect::<String>()
                ));
            }
            Some(Setting {
                value: level.to_owned(),
                source: Source::Override,
            })
        };
        self.commands[self.selected] = match &setting {
            Some(setting) => self.base_commands[self.selected].with_effort(&setting.value),
            None => self.base_commands[self.selected].clone(),
        };
        self.effort_overrides[self.selected] = setting;
        self.effort_picker = None;
        self.notice = None;
        Ok(())
    }

    pub fn save_effort(&mut self) -> Result<(), String> {
        if self.is_done() {
            return Err(format!("the launcher is already {}", self.status.name()));
        }
        let effort = self.effort_overrides[self.selected]
            .as_ref()
            .ok_or("choose an effort override before saving")?
            .value
            .clone();
        profile::save_effort(self.selected_profile(), &effort)
            .map_err(|error| format!("{error:#}"))?;
        let refreshed = profile::load(&self.catalog.codex_home)
            .map_err(|error| format!("effort saved, but cannot reload profiles: {error:#}"))?;
        // Keep selections and other profiles' temporary overrides in place.
        for (index, profile) in self.catalog.profiles.iter_mut().enumerate() {
            if let Some(updated) = refreshed.find(&profile.name) {
                if profile.model != updated.model
                    || profile.provider != updated.provider
                    || profile.model_catalog != updated.model_catalog
                {
                    self.effort_choices[index] =
                        Choices::load(updated, &self.catalog.codex_home, &self.extra_args);
                    self.effort_queried[index] = false;
                }
                *profile = updated.clone();
            }
        }
        self.catalog.warnings = refreshed.warnings;
        // Retain the override if launch arguments would otherwise supersede
        // the value just saved. Clearing it still restores the original args.
        if self.argument_effort.is_none() {
            self.set_effort("profile")?;
        }
        self.effort_picker = None;
        self.notice = Some(format!(
            "Saved effort {effort} to {}",
            self.selected_profile().path.display()
        ));
        Ok(())
    }

    fn open_effort_picker(&mut self) {
        self.discover_effort();
        let selected = self.effort_overrides[self.selected]
            .as_ref()
            .and_then(|setting| {
                self.effort_choices()
                    .levels
                    .iter()
                    .position(|s| s == &setting.value)
            })
            .map_or(0, |index| index + 1);
        self.effort_picker = Some(EffortPicker { selected });
        self.notice = None;
    }

    fn press_effort_picker(&mut self, key: Key) {
        let last = self.effort_choices().levels.len();
        let picker = self.effort_picker.as_mut().unwrap();
        match key {
            Key::Up | Key::Char('k') => picker.selected = picker.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => picker.selected = (picker.selected + 1).min(last),
            Key::Home | Key::Char('g') => picker.selected = 0,
            Key::End | Key::Char('G') => picker.selected = last,
            Key::Esc | Key::Char('q') => self.effort_picker = None,
            Key::Ctrl('c') => self.apply(Action::Quit),
            Key::Enter | Key::Char('s') => {
                let selected = picker.selected;
                if key == Key::Char('s') && selected == 0 {
                    self.notice = Some("Choose an explicit effort level to save.".to_owned());
                    return;
                }
                let level = if selected == 0 {
                    "profile".to_owned()
                } else {
                    self.effort_choices().levels[selected - 1].clone()
                };
                let result = self.set_effort(&level).and_then(|()| {
                    if key == Key::Char('s') {
                        self.save_effort()
                    } else {
                        Ok(())
                    }
                });
                if let Err(error) = result {
                    self.notice = Some(error);
                }
            }
            _ => {}
        }
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
        if self.is_done() {
            return;
        }
        if self.effort_picker.is_some() {
            self.press_effort_picker(key);
            return;
        }
        if self.help.is_some() {
            self.press_help(key);
            return;
        }
        if let Some(action) = action_for(key) {
            self.apply(action);
        }
    }

    /// While the help is open, keys scroll or close it, and Ctrl-C quits.
    fn press_help(&mut self, key: Key) {
        let help = self.help.as_mut().unwrap();
        match key {
            Key::Up | Key::Char('k') => help.up(1),
            Key::Down | Key::Char('j') => help.down(1),
            Key::PageUp => help.up(help.page_step()),
            Key::PageDown => help.down(help.page_step()),
            Key::Home | Key::Char('g') => help.offset = 0,
            Key::End | Key::Char('G') => help.offset = help.max_offset,
            Key::Char('?') | Key::Enter | Key::Esc | Key::Char('q') => self.help = None,
            Key::Ctrl('c') => self.apply(Action::Quit),
            _ => {}
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
            Action::ScrollUp => self.view.details.up(self.view.details.page_step()),
            Action::ScrollDown => self.view.details.down(self.view.details.page_step()),
            Action::EditEffort => self.open_effort_picker(),
            Action::SaveEffort => {
                if let Err(error) = self.save_effort() {
                    self.notice = Some(error);
                }
            }
            Action::Recheck => self.check_servers(),
            Action::Help => {
                self.help = Some(Scroll::default());
                self.notice = None;
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

    fn select(&mut self, index: usize) {
        self.effort_picker = None;
        self.notice = None;
        if index != self.selected {
            self.selected = index;
            self.view.details.offset = 0;
        }
    }

    /// The state that `ctl` reports.
    pub fn state<'a>(&'a self, instance: &'a Instance) -> StateView<'a> {
        let command = self.command();
        let mut profile = ProfileView::new(self.selected_profile(), command, false);
        profile.effort = self.effective_effort();
        profile.server = self.server_status();
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
            menu: self
                .catalog
                .profiles
                .iter()
                .enumerate()
                .map(|(index, profile)| {
                    let mut entry = MenuEntry::new(profile);
                    entry.effort = self.effort_at(index).map(|setting| setting.value.as_str());
                    entry.server = self.server_status_at(index).map(|status| status.state);
                    entry
                })
                .collect(),
            profile,
            effort_choices: self.effort_choices(),
            effort_picker: self.effort_picker.as_ref(),
            effort_override: self.effort_overrides[self.selected]
                .as_ref()
                .map(|s| s.value.as_str()),
            help: self.help.as_ref(),
            notice: self.notice.as_deref(),
            details: &self.view.details,
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
    pub effort_choices: &'a Choices,
    pub effort_picker: Option<&'a EffortPicker>,
    pub effort_override: Option<&'a str>,
    pub help: Option<&'a Scroll>,
    pub notice: Option<&'a str>,
    pub details: &'a Scroll,
    pub warnings: &'a [String],
}

/// One menu row.
#[derive(Serialize)]
pub struct MenuEntry<'a> {
    pub name: &'a str,
    pub model: Option<&'a str>,
    pub provider: &'a str,
    pub effort: Option<&'a str>,
    /// The server check's state, once checks run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<State>,
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
            server: None,
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
    /// The server check, once checks run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<&'a check::Status>,
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
            server: None,
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
    fn retrying_discovery_updates_an_open_pickers_selection() {
        use std::os::unix::fs::PermissionsExt;
        let (home, _) = fixture_app();
        let bin = tempfile::tempdir().unwrap();
        let codex = bin.path().join("codex");
        std::fs::write(&codex, "#!/bin/sh\nprintf '%s' '{\"models\":[{\"slug\":\"gpt-base\",\"supported_reasoning_levels\":[{\"effort\":\"high\"}]}]}'\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut app = App::new(
            profile::load(home.path()).unwrap(),
            &[],
            Some(bin.path().as_os_str()),
            None,
        );
        app.set_discovery_deadline(Some(Instant::now()));
        app.press(Key::Char('e'));
        app.press(Key::End);
        assert!(app.effort_picker.as_ref().unwrap().selected > 1);
        app.set_discovery_deadline(None);
        app.press(Key::Enter);
        assert_eq!(app.effort_choices().levels, ["high"]);
        assert_eq!(app.effort_picker.as_ref().unwrap().selected, 0);
        assert!(!app.has_effort_override());
        app.press(Key::Enter);
        assert!(app.effort_picker.is_none());
        assert_eq!(app.status(), &Status::Selecting);
    }

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
        app.view.details.fit(4, 14);
        app.press(Key::PageDown);
        assert_eq!(app.view.details.offset, 3);
        app.press(Key::PageDown);
        app.press(Key::PageDown);
        app.press(Key::PageDown);
        assert_eq!(app.view.details.offset, 10);
        app.press(Key::PageUp);
        assert_eq!(app.view.details.offset, 7);
        app.press(Key::Down);
        assert_eq!(app.view.details.offset, 0);
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
    fn help_takes_keys_until_it_closes() {
        let (_home, mut app) = fixture_app();
        app.press(Key::Char('?'));
        // Three visible lines of eight.
        app.help.as_mut().unwrap().fit(3, 8);
        for (key, offset) in [
            (Key::Down, 1),
            (Key::PageDown, 3),
            (Key::End, 5),
            (Key::Char('k'), 4),
            (Key::PageUp, 2),
            (Key::Home, 0),
        ] {
            app.press(key);
            assert_eq!(app.help.as_ref().unwrap().offset, offset, "{key:?}");
        }
        // Menu keys do nothing while the help is open.
        app.press(Key::Char('e'));
        app.press(Key::Char('s'));
        assert!(app.effort_picker.is_none());
        assert!(app.notice.is_none());
        let instance = Instance {
            pid: 42,
            mode: "headless",
            tty: None,
            cwd: None,
        };
        let state = serde_json::to_value(app.state(&instance)).unwrap();
        assert_eq!(
            state["help"],
            serde_json::json!({"scroll": 0, "max_scroll": 5})
        );
        assert_eq!(state["selected"], "default");
        assert!(app.help.is_some());
        app.press(Key::Ctrl('c'));
        assert_eq!(app.status(), &Status::Quitting);

        for key in [Key::Char('?'), Key::Enter, Key::Esc, Key::Char('q')] {
            let (_home, mut app) = fixture_app();
            app.press(Key::Char('?'));
            app.press(key);
            assert!(app.help.is_none(), "{key:?}");
            assert_eq!(app.status(), &Status::Selecting, "{key:?}");
            let state = serde_json::to_value(app.state(&instance)).unwrap();
            assert!(state["help"].is_null(), "{key:?}");
        }

        // The effort picker does not hand ? to the menu.
        let (_home, mut app) = fixture_app();
        app.press(Key::Char('e'));
        app.press(Key::Char('?'));
        assert!(app.help.is_none());
        assert!(app.effort_picker.is_some());
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
