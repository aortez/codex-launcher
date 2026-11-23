//! Launcher state, keys, and the actions that change it.
//!
//! The terminal and the control socket share this state and keymap, so a
//! `ctl key` request behaves exactly like the same key typed in the menu.

use std::ffi::OsStr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use serde::Serialize;

use crate::check::{self, Checks, State};
use crate::effort::{self, Choices};
use crate::launch::{Command, Program};
use crate::profile::{self, Catalog, Profile, Provider, Setting, Source};
use crate::sessions::{self, Entry, Loader, Match, NotFound, Session, Sessions};

/// The Codex subcommand that resumes a session.
const RESUME: &str = "resume";

/// Why the sessions view cannot resume a session with these launch arguments.
pub const LAUNCH_ARGUMENTS_NOTICE: &str =
    "Sessions resume only without launch arguments, or with a bare \"resume\".";

/// Why Enter does nothing yet: the sessions decide what it runs.
const STILL_READING: &str = "The sessions are still being read.";

/// Built-in providers whose servers load whichever model a request names,
/// usually on this machine.
const LOCAL_PROVIDERS: &[&str] = &["lmstudio", "ollama"];

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
    /// Switch between the profiles and the sessions.
    SwitchView,
    /// Switch the sessions between the current directory and every directory.
    SwitchDirectories,
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
        Key::Tab => Action::SwitchView,
        Key::Char('a') => Action::SwitchDirectories,
        Key::Char('?') => Action::Help,
        Key::Enter => Action::Launch,
        Key::Esc | Key::Char('q') | Key::Ctrl('c') => Action::Quit,
        _ => return None,
    })
}

/// Which list the menu shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Page {
    Profiles,
    Sessions,
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
    pub sessions: TableState,
    /// The rows that the sessions table showed when last drawn, which
    /// `ctl state` reports.
    pub session_rows: Range<usize>,
    pub session_details: Scroll,
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
    page: Page,
    /// The sessions, from when the sessions view first opens or `-- resume`
    /// names one.
    sessions: Option<SessionList>,
    /// The directory whose sessions the view lists, unless it shows them all.
    current_dir: Option<PathBuf>,
    /// The session that `-- resume <arg>` names, until the sessions arrive to
    /// say which profile resumes it.
    resume_target: Option<String>,
    /// Whether `-- resume <name>` named several sessions. Resuming one from
    /// the sessions view then puts its ID in place of the name.
    target_shared: bool,
    pub profile_picker: Option<ProfilePicker>,
}

#[derive(Debug, Serialize)]
pub struct EffortPicker {
    /// Zero is inheritance; subsequent entries index the available levels.
    pub selected: usize,
}

/// The profiles that Enter offers when several can resume a session.
#[derive(Debug)]
pub struct ProfilePicker {
    /// The session, by its index among the sessions read.
    entry: usize,
    /// Indexes of the matching profiles.
    pub profiles: Vec<usize>,
    pub selected: usize,
    /// Whether the picked profile runs with the launch arguments as given,
    /// since `-- resume <session>` already names the session.
    keeps_arguments: bool,
}

/// The sessions view's list.
pub struct SessionList {
    /// Reading the sessions, until they arrive.
    loader: Option<Loader>,
    sessions: Sessions,
    /// Whether the list shows every directory rather than the current one.
    all: bool,
    /// The name that `-- resume <name>` found on several sessions, while the
    /// list shows only them.
    named: Option<String>,
    /// Indexes into `sessions.entries` of the rows shown.
    rows: Vec<usize>,
    /// The selected row.
    selected: usize,
}

impl SessionList {
    pub fn is_loading(&self) -> bool {
        self.loader.is_some()
    }

    pub fn shows_all(&self) -> bool {
        self.all
    }

    /// The name that every listed session shares, when the list shows only
    /// the sessions that `-- resume <name>` names.
    pub fn named(&self) -> Option<&str> {
        self.named.as_deref()
    }

    /// Which sessions the list shows, as `ctl state` reports it.
    fn scope(&self) -> &'static str {
        match (&self.named, self.all) {
            (Some(_), _) => "named",
            (None, true) => "all",
            (None, false) => "directory",
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.sessions.warnings
    }

    /// The entries shown, newest first.
    pub fn rows(&self) -> impl Iterator<Item = &Entry> {
        self.rows.iter().map(|&index| &self.sessions.entries[index])
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The selected row.
    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        let index = *self.rows.get(self.selected)?;
        Some(&self.sessions.entries[index])
    }

    /// Choose the rows for the current scope, keeping the selected entry when
    /// it is still shown.
    fn refresh(&mut self) {
        let selected = self.rows.get(self.selected).copied();
        self.rows = match &self.named {
            Some(name) => self.sessions.named(name).collect(),
            None => (0..self.sessions.entries.len())
                .filter(|&index| self.sessions.entries[index].is_listed(self.all))
                .collect(),
        };
        self.selected = selected
            .and_then(|entry| self.rows.iter().position(|&row| row == entry))
            .unwrap_or(0);
    }
}

/// What Enter does in the sessions view.
enum Resume {
    Launch(Command),
    /// Several profiles match, so the user picks one.
    Pick(ProfilePicker),
    /// The selected entry cannot be resumed, for this reason.
    Refuse(String),
    /// No row is selected.
    Nothing,
}

impl App {
    /// Create the menu with the first entry selected.
    ///
    /// `extra_args` are appended to every launch command, and `path` is the
    /// search path for `codex-<profile>` wrappers. A bare `resume` opens the
    /// sessions view, and `resume <session>` selects the session's profile
    /// once the sessions have been read.
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
        // A flag after `resume`, as in `resume --last`, leaves the choice to Codex.
        let (page, resume_target) = match extra_args {
            [resume] if *resume == RESUME => (Page::Sessions, None),
            [resume, target, ..] if *resume == RESUME && !target.starts_with('-') => {
                (Page::Profiles, Some(target.clone()))
            }
            _ => (Page::Profiles, None),
        };
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
            page,
            sessions: None,
            current_dir: std::env::current_dir().ok(),
            resume_target,
            target_shared: false,
            profile_picker: None,
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Which list the menu shows.
    pub fn page(&self) -> Page {
        self.page
    }

    /// The sessions view's list, once it has started loading.
    pub fn session_list(&self) -> Option<&SessionList> {
        self.sessions.as_ref()
    }

    /// The directory whose sessions the view lists, unless it shows them all.
    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// Read sessions as if the launcher ran in `dir`.
    #[cfg(test)]
    pub fn set_current_dir(&mut self, dir: &Path) {
        self.current_dir = Some(dir.to_path_buf());
    }

    /// The profiles that can resume `session`.
    pub fn session_match(&self, session: &Session) -> Match {
        sessions::matching(session, &self.catalog)
    }

    /// The command that resumes `session` with the profile at `index`: the
    /// profile's launch command, then `resume <id>`. None when the launch
    /// arguments already say what to run.
    pub fn session_command(&self, session: &Session, index: usize) -> Option<Command> {
        let args = self.resume_arguments(&session.id)?;
        let mut command = self.base_commands[index].clone();
        // The launch arguments come last, and these take their place.
        command
            .args
            .truncate(command.args.len() - self.extra_args.len());
        command.args.extend(args);
        Some(match &self.effort_overrides[index] {
            Some(setting) => command.with_effort(&setting.value),
            None => command,
        })
    }

    /// The launch arguments that resume the session `id`, or None when the
    /// launch arguments already say what to run. No launch arguments, or a
    /// bare `resume`, become `resume <id>`, and the ID replaces a name that
    /// several sessions share.
    fn resume_arguments(&self, id: &str) -> Option<Vec<String>> {
        let resume = || [RESUME.to_owned(), id.to_owned()].into_iter();
        match self.extra_args.as_slice() {
            [] => Some(resume().collect()),
            [first] if *first == RESUME => Some(resume().collect()),
            [first, _name, rest @ ..] if *first == RESUME && self.target_shared => {
                Some(resume().chain(rest.iter().cloned()).collect())
            }
            _ => None,
        }
    }

    /// Whether launching the profile at `index` can start another model on a
    /// server you run: it launches through a wrapper, or its provider loads
    /// whichever model a request names.
    pub fn launch_loads_model(&self, index: usize) -> bool {
        self.commands[index].wrapper.is_some()
            || LOCAL_PROVIDERS.contains(&self.catalog.profiles[index].provider.id.as_str())
    }

    /// The session that the open profile picker resumes.
    pub fn picker_session(&self) -> Option<&Session> {
        let picker = self.profile_picker.as_ref()?;
        self.sessions.as_ref()?.sessions.entries[picker.entry].session()
    }

    /// What launching the open picker's highlighted profile runs.
    fn picker_command(&self) -> Option<Command> {
        let picker = self.profile_picker.as_ref()?;
        let index = picker.profiles[picker.selected];
        if picker.keeps_arguments {
            return Some(self.commands[index].clone());
        }
        self.session_command(self.picker_session()?, index)
    }

    /// Start reading sessions once something needs them, and show them once
    /// they arrive. Returns whether the screen changed.
    pub fn poll_sessions(&mut self) -> bool {
        if self.page == Page::Sessions || self.resume_target.is_some() {
            self.start_sessions();
        }
        let Some(list) = &mut self.sessions else {
            return false;
        };
        let Some(sessions) = list.loader.as_ref().and_then(Loader::poll) else {
            return false;
        };
        list.loader = None;
        list.sessions = sessions;
        list.refresh();
        self.view.session_details.offset = 0;
        if let Some(target) = self.resume_target.take() {
            self.resolve_resume(&target);
        }
        true
    }

    fn start_sessions(&mut self) {
        if self.sessions.is_none() {
            self.sessions = Some(SessionList {
                loader: Some(Loader::start(
                    self.catalog.codex_home.clone(),
                    self.current_dir.clone(),
                )),
                sessions: Sessions::default(),
                all: false,
                named: None,
                rows: Vec::new(),
                selected: 0,
            });
        }
    }

    /// Select the profile that resumes the session `target` names, or offer
    /// the choices when there are several. When there is none, `default`
    /// stays selected, since Codex may know the session or the provider.
    fn resolve_resume(&mut self, target: &str) {
        let Some(list) = &mut self.sessions else {
            return;
        };
        let entry = match list.sessions.find(target) {
            Ok(entry) => entry,
            Err(NotFound::Missing(notice)) => {
                self.notice = Some(notice);
                return;
            }
            Err(NotFound::Shared(count)) => {
                // The sessions view lists them, and the one chosen there
                // resumes by its ID instead of the name.
                list.named = Some(target.to_owned());
                list.refresh();
                list.selected = 0;
                self.target_shared = true;
                self.page = Page::Sessions;
                self.notice = Some(format!(
                    "{count} sessions are named {:?}; choose one.",
                    sessions::clean(target)
                ));
                return;
            }
        };
        let Some(session) = list.sessions.entries[entry].session() else {
            return;
        };
        let title = sessions::clean(session.title());
        let provider = sessions::clean(&session.provider);
        let profiles = sessions::matching(session, &self.catalog).profiles;
        match profiles.as_slice() {
            &[index] => {
                self.select(index);
                self.notice = Some(format!(
                    "Selected {} for session {title:?}.",
                    self.catalog.profiles[index].name
                ));
            }
            [] => {
                self.notice = Some(format!(
                    "No profile can resume {title:?}: it needs provider {provider}."
                ));
            }
            &[first, ..] => {
                // Dismissing the picker leaves a profile selected that can
                // resume the session.
                self.select(first);
                self.profile_picker = Some(ProfilePicker {
                    entry,
                    profiles,
                    selected: 0,
                    keeps_arguments: true,
                });
            }
        }
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
        if self.catalog.profiles[index].harness.is_some() {
            return self.catalog.profiles[index].effort.as_ref();
        }
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
        if self.selected_profile().harness.is_some() {
            return format!(
                "Use harness preset ({})",
                self.effective_effort()
                    .map_or("unspecified", |s| s.value.as_str())
            );
        }
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
        if self.profile_picker.is_some() {
            self.press_profile_picker(key);
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

    fn press_profile_picker(&mut self, key: Key) {
        let picker = self.profile_picker.as_mut().unwrap();
        let last = picker.profiles.len() - 1;
        match key {
            Key::Up | Key::Char('k') => picker.selected = picker.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => picker.selected = (picker.selected + 1).min(last),
            Key::Home | Key::Char('g') => picker.selected = 0,
            Key::End | Key::Char('G') => picker.selected = last,
            Key::Enter => self.launch_picked(),
            Key::Esc | Key::Char('q') => self.profile_picker = None,
            Key::Ctrl('c') => self.apply(Action::Quit),
            _ => {}
        }
    }

    /// Resume the open picker's session with its highlighted profile.
    fn launch_picked(&mut self) {
        let Some(command) = self.picker_command() else {
            return;
        };
        if let Some(picker) = self.profile_picker.take()
            && picker.keeps_arguments
        {
            // The picker chose among the profiles in the menu, so select the
            // one launched.
            self.select(picker.profiles[picker.selected]);
        }
        self.status = Status::Launching(command);
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
        if action == Action::Launch && self.profile_picker.is_some() {
            self.launch_picked();
            return;
        }
        if self.page == Page::Sessions {
            self.apply_to_sessions(action);
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
            Action::SwitchView => self.switch_view(),
            // Only the sessions view has directories to switch between.
            Action::SwitchDirectories => {}
            Action::Help => self.open_help(),
            Action::Launch => match self.enter_command() {
                Some(command) => self.status = Status::Launching(command),
                // `-- resume <session>` may still select another profile.
                None => self.notice = Some(STILL_READING.to_owned()),
            },
            Action::Quit => self.status = Status::Quitting,
        }
    }

    fn apply_to_sessions(&mut self, action: Action) {
        let rows = self.sessions.as_ref().map_or(0, SessionList::row_count);
        let selected = self.sessions.as_ref().map_or(0, SessionList::selected);
        match action {
            Action::Previous => self.select_session(selected.saturating_sub(1)),
            Action::Next => self.select_session(selected + 1),
            Action::First => self.select_session(0),
            Action::Last => self.select_session(rows.saturating_sub(1)),
            Action::ScrollUp => {
                let details = &mut self.view.session_details;
                details.up(details.page_step());
            }
            Action::ScrollDown => {
                let details = &mut self.view.session_details;
                details.down(details.page_step());
            }
            // Effort belongs to a profile, and a session may match several.
            Action::EditEffort | Action::SaveEffort => {}
            Action::Recheck => self.check_servers(),
            Action::SwitchView => self.switch_view(),
            Action::SwitchDirectories => self.switch_directories(),
            Action::Help => self.open_help(),
            Action::Launch => match self.resume() {
                Resume::Launch(command) => self.status = Status::Launching(command),
                Resume::Pick(picker) => {
                    self.profile_picker = Some(picker);
                    self.notice = None;
                }
                Resume::Refuse(notice) => self.notice = Some(notice),
                Resume::Nothing => {}
            },
            Action::Quit => self.status = Status::Quitting,
        }
    }

    fn open_help(&mut self) {
        self.help = Some(Scroll::default());
        self.notice = None;
    }

    fn switch_view(&mut self) {
        self.page = match self.page {
            Page::Profiles => Page::Sessions,
            Page::Sessions => Page::Profiles,
        };
        self.notice = None;
        if self.page == Page::Sessions {
            self.start_sessions();
        }
    }

    fn switch_directories(&mut self) {
        self.notice = None;
        let Some(list) = &mut self.sessions else {
            return;
        };
        let before = list.rows.get(list.selected).copied();
        // Sessions that share a name can come from any directory, so every
        // directory follows them.
        list.all = list.named.take().is_some() || !list.all;
        list.refresh();
        if list.rows.get(list.selected).copied() != before {
            self.view.session_details.offset = 0;
        }
    }

    fn select_session(&mut self, row: usize) {
        self.notice = None;
        let Some(list) = &mut self.sessions else {
            return;
        };
        let row = row.min(list.rows.len().saturating_sub(1));
        if row != list.selected {
            list.selected = row;
            self.view.session_details.offset = 0;
        }
    }

    /// What Enter does in the sessions view, for the selected row.
    fn resume(&self) -> Resume {
        let Some(list) = &self.sessions else {
            return Resume::Nothing;
        };
        if list.is_loading() {
            return Resume::Refuse(STILL_READING.to_owned());
        }
        let Some(&entry) = list.rows.get(list.selected) else {
            return Resume::Nothing;
        };
        let Entry::Session(session) = &list.sessions.entries[entry] else {
            return Resume::Refuse(
                "This log could not be read, so it cannot be resumed.".to_owned(),
            );
        };
        let matched = sessions::matching(session, &self.catalog);
        if matched.profiles.is_empty() {
            return Resume::Refuse(format!(
                "No profile can resume this session: it needs provider {}.",
                sessions::clean(&session.provider)
            ));
        }
        if self.resume_arguments(&session.id).is_none() {
            return Resume::Refuse(LAUNCH_ARGUMENTS_NOTICE.to_owned());
        }
        match matched.profiles.as_slice() {
            &[index] => self
                .session_command(session, index)
                .map_or(Resume::Nothing, Resume::Launch),
            several => Resume::Pick(ProfilePicker {
                entry,
                profiles: several.to_vec(),
                selected: 0,
                keeps_arguments: false,
            }),
        }
    }

    /// The command that Enter runs now, if it runs one.
    fn enter_command(&self) -> Option<Command> {
        if self.profile_picker.is_some() {
            return self.picker_command();
        }
        match self.page {
            // Until the sessions arrive, `-- resume <session>` may still
            // select another profile.
            Page::Profiles => self.resume_target.is_none().then(|| self.command().clone()),
            Page::Sessions => match self.resume() {
                Resume::Launch(command) => Some(command),
                _ => None,
            },
        }
    }

    /// Move the selection to the profile called `name`, in the profiles view.
    pub fn select_name(&mut self, name: &str) -> Result<(), String> {
        if self.is_done() {
            return Err(format!("the launcher is already {}", self.status.name()));
        }
        let index = self
            .catalog
            .index_of(name)
            .ok_or_else(|| unknown_profile(&self.catalog, name))?;
        self.select(index);
        self.page = Page::Profiles;
        self.profile_picker = None;
        Ok(())
    }

    fn select(&mut self, index: usize) {
        self.effort_picker = None;
        self.notice = None;
        // The user's own choice wins over a session that is still being found.
        self.resume_target = None;
        if index != self.selected {
            self.selected = index;
            self.view.details.offset = 0;
        }
    }

    /// The state that `ctl` reports.
    pub fn state<'a>(&'a self, instance: &'a Instance) -> StateView<'a> {
        let mut profile = ProfileView::new(self.selected_profile(), self.command(), false);
        profile.effort = self.effective_effort();
        profile.server = self.server_status();
        let command = match &self.status {
            Status::Launching(command) => Some(command.clone()),
            _ => self.enter_command(),
        };
        StateView {
            pid: instance.pid,
            mode: instance.mode,
            tty: instance.tty.as_deref(),
            cwd: instance.cwd.as_deref(),
            codex_home: self.catalog.codex_home.display().to_string(),
            status: self.status.name(),
            view: self.page,
            selected: &self.selected_profile().name,
            selected_index: self.selected,
            command: command.as_ref().map(Command::argv),
            command_line: command.as_ref().map(Command::display),
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
            profile_picker: self
                .profile_picker
                .as_ref()
                .map(|picker| ProfilePickerView {
                    session: self
                        .picker_session()
                        .map_or("", |session| session.id.as_str()),
                    profiles: picker
                        .profiles
                        .iter()
                        .map(|&index| self.catalog.profiles[index].name.as_str())
                        .collect(),
                    selected: picker.selected,
                }),
            help: self.help.as_ref(),
            notice: self.notice.as_deref(),
            details: match self.page {
                Page::Profiles => &self.view.details,
                Page::Sessions => &self.view.session_details,
            },
            warnings: &self.catalog.warnings,
            sessions: self.sessions.as_ref().map(|list| {
                // Only the rows on screen, so that a long history does not
                // lengthen every reply.
                let end = self.view.session_rows.end.min(list.rows.len());
                let start = self.view.session_rows.start.min(end);
                SessionsView {
                    loading: list.is_loading(),
                    scope: list.scope(),
                    selected_index: (!list.rows.is_empty()).then_some(list.selected),
                    total: list.rows.len(),
                    offset: start,
                    list: list.rows[start..end]
                        .iter()
                        .map(|&index| self.session_row(&list.sessions.entries[index]))
                        .collect(),
                    warnings: list.warnings(),
                }
            }),
        }
    }

    fn session_row<'a>(&'a self, entry: &'a Entry) -> SessionRow<'a> {
        match entry {
            Entry::Session(session) => {
                let matched = self.session_match(session);
                SessionRow {
                    id: Some(&session.id),
                    title: session.title(),
                    name: session.name.as_deref(),
                    branch: session.branch.as_deref(),
                    directory: Some(session.cwd.display().to_string()),
                    updated: sessions::utc(session.updated),
                    model: session.model.as_deref(),
                    provider: Some(&session.provider),
                    profiles: matched
                        .profiles
                        .iter()
                        .map(|&index| self.catalog.profiles[index].name.as_str())
                        .collect(),
                    model_differs: matched.model_differs,
                    logs: session
                        .logs
                        .iter()
                        .map(|log| log.display().to_string())
                        .collect(),
                    error: None,
                }
            }
            Entry::Unknown(unknown) => SessionRow {
                id: None,
                title: "unknown",
                name: None,
                branch: None,
                directory: unknown.cwd.as_ref().map(|cwd| cwd.display().to_string()),
                updated: sessions::utc(unknown.updated),
                model: None,
                provider: None,
                profiles: Vec::new(),
                model_differs: false,
                logs: vec![unknown.path.display().to_string()],
                error: Some(&unknown.error),
            },
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
    /// Which list the menu shows.
    pub view: Page,
    pub selected: &'a str,
    pub selected_index: usize,
    /// What Enter runs, or what is being launched. Null when Enter runs
    /// nothing at once, as when it asks which profile resumes a session.
    pub command: Option<Vec<String>>,
    pub command_line: Option<String>,
    pub menu: Vec<MenuEntry<'a>>,
    pub profile: ProfileView<'a>,
    pub effort_choices: &'a Choices,
    pub effort_picker: Option<&'a EffortPicker>,
    pub effort_override: Option<&'a str>,
    pub profile_picker: Option<ProfilePickerView<'a>>,
    pub help: Option<&'a Scroll>,
    pub notice: Option<&'a str>,
    /// The scroll of the details pane in view.
    pub details: &'a Scroll,
    pub warnings: &'a [String],
    /// Null until the sessions view first opens or `-- resume` names a
    /// session.
    pub sessions: Option<SessionsView<'a>>,
}

/// The profile picker, as `ctl state` reports it.
#[derive(Serialize)]
pub struct ProfilePickerView<'a> {
    /// The ID of the session to resume.
    pub session: &'a str,
    pub profiles: Vec<&'a str>,
    pub selected: usize,
}

/// The sessions view, as `ctl state` reports it.
#[derive(Serialize)]
pub struct SessionsView<'a> {
    /// Whether the sessions are still being read.
    pub loading: bool,
    /// `directory` for the current directory's sessions, `all`, or `named`
    /// for the sessions that share the name that `-- resume` gave.
    pub scope: &'static str,
    /// The selected row, counting from the first of `total`, or null when
    /// there are none.
    pub selected_index: Option<usize>,
    /// How many rows the list has.
    pub total: usize,
    /// The row that `list` starts with.
    pub offset: usize,
    /// The rows that the table shows, newest first.
    pub list: Vec<SessionRow<'a>>,
    pub warnings: &'a [String],
}

/// One row of the sessions view.
#[derive(Serialize)]
pub struct SessionRow<'a> {
    /// Null for a log that could not be read.
    pub id: Option<&'a str>,
    /// What the row shows as the name: the session's name, its git branch,
    /// or its ID, or `unknown` for a log that could not be read.
    pub title: &'a str,
    pub name: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub directory: Option<String>,
    /// When the newest log changed, in UTC.
    pub updated: String,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    /// The profiles that can resume it. With several, Enter asks which.
    pub profiles: Vec<&'a str>,
    /// Whether every matching profile uses a model other than the session's.
    pub model_differs: bool,
    pub logs: Vec<String>,
    /// Why the log could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
}

/// One menu row.
#[derive(Serialize)]
pub struct MenuEntry<'a> {
    pub name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<&'a str>,
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
            harness: profile.harness.as_ref().map(|h| h.label.as_str()),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<&'a str>,
    pub path: String,
    pub description: Option<&'a str>,
    pub model: Option<&'a Setting>,
    pub provider: &'a Provider,
    /// The server check, once checks run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<&'a check::Status>,
    pub effort: Option<&'a Setting>,
    pub context_window: Option<&'a Setting>,
    pub auto_compact_token_limit: Option<&'a Setting>,
    pub sandbox: Option<&'a Setting>,
    pub approval: Option<&'a Setting>,
    pub command: Vec<String>,
    pub command_line: String,
    pub wrapper: Option<String>,
    /// The status command that server checks run, when there is one, as a
    /// shell-quoted line.
    pub status_command: Option<String>,
    pub error: Option<&'a str>,
    /// The redacted file text, included only on request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<&'a str>,
}

impl<'a> ProfileView<'a> {
    pub fn new(profile: &'a Profile, command: &Command, with_file: bool) -> Self {
        Self {
            name: &profile.name,
            harness: profile.harness.as_ref().map(|h| h.label.as_str()),
            tools: profile.harness.as_ref().and_then(|h| h.tools.as_deref()),
            path: profile.path.display().to_string(),
            description: profile.description.as_deref(),
            model: profile.model.as_ref(),
            provider: &profile.provider,
            server: None,
            effort: profile.effort.as_ref(),
            context_window: profile.context_window.as_ref(),
            auto_compact_token_limit: profile.auto_compact_token_limit.as_ref(),
            sandbox: profile.sandbox.as_ref(),
            approval: profile.approval.as_ref(),
            command: command.argv(),
            command_line: command.display(),
            wrapper: command
                .wrapper
                .as_ref()
                .map(|path| path.display().to_string()),
            status_command: command.status.as_ref().map(Program::display),
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
    fn a_choice_made_while_sessions_load_wins_over_the_resume_argument() {
        let home = crate::test_support::fixture_home();
        let args = ["resume".to_owned(), "Fix the flaky tests".to_owned()];
        let mut app = App::new(profile::load(home.path()).unwrap(), &args, None, None);
        // The first poll starts reading, and only a later one applies them.
        app.poll_sessions();
        app.press(Key::End);
        crate::test_support::wait_for_sessions(&mut app);
        assert_eq!(app.selected_profile().name, "local");
        assert!(app.notice.is_none());

        let mut app = App::new(profile::load(home.path()).unwrap(), &args, None, None);
        crate::test_support::wait_for_sessions(&mut app);
        assert_eq!(app.selected_profile().name, "gateway");
    }

    #[test]
    fn enter_waits_for_the_session_that_resume_names() {
        let home = crate::test_support::fixture_home();
        let args = ["resume".to_owned(), "Fix the flaky tests".to_owned()];
        let mut app = App::new(profile::load(home.path()).unwrap(), &args, None, None);
        let instance = Instance {
            pid: 42,
            mode: "headless",
            tty: None,
            cwd: None,
        };
        // Until the sessions arrive, default is selected but may not resume
        // the session.
        assert!(app.state(&instance).command.is_none());
        app.press(Key::Enter);
        assert_eq!(app.status(), &Status::Selecting);
        assert_eq!(app.notice.as_deref(), Some(STILL_READING));

        crate::test_support::wait_for_sessions(&mut app);
        app.press(Key::Enter);
        let Status::Launching(command) = app.status() else {
            panic!("not launching: {:?}", app.status());
        };
        assert_eq!(
            command.argv(),
            [
                "codex",
                "--profile",
                "gateway",
                "resume",
                "Fix the flaky tests"
            ]
        );
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
