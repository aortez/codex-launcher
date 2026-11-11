//! Drawing the menu and turning a frame into plain text.

use std::path::Path;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Scroll};
use crate::check::{State, Status};
use crate::profile::{Profile, Setting, Source};

const LABEL_WIDTH: usize = 10;
const MIN_LIST_WIDTH: u16 = 14;
const MAX_LIST_WIDTH: u16 = 32;
/// A server mark and the space after it, shown once checks start.
const MARK_WIDTH: u16 = 2;
/// The main menu's key hints, after "↑/↓ move".
const HINTS: &str = "e effort · r recheck · Enter launch · PgUp/PgDn scroll · ? help · q quit";
/// The key hints while an effort override is unsaved, after "↑/↓ move".
const OVERRIDE_HINTS: &str = "e effort · s save effort · Enter launch · ? help · q quit";
/// The narrowest help meaning column worth keeping beside its name.
const MIN_MEANING_WIDTH: usize = 20;

/// What each server mark means, in the order the help lists them.
const MARKS: [(State, &str); 6] = [
    (State::Ready, "The server lists the profile's model."),
    (
        State::ModelMissing,
        "The server answered without the model.",
    ),
    (
        State::Reachable,
        "The server answered, but not with a model list.",
    ),
    (
        State::Down,
        "No answer, a 5xx error, or refused credentials.",
    ),
    (State::Checking, "The server has not answered yet."),
    (State::Skipped, "The details pane's server line says why."),
];

/// The menu's keys, as the help lists them.
const KEYS: [(&str, &str); 9] = [
    ("↑/↓ or k/j", "Move the selection."),
    ("Home/End or g/G", "Select the first or last profile."),
    ("PgUp/PgDn", "Scroll the details."),
    ("e", "Choose the reasoning effort."),
    ("s", "Save the chosen effort to the profile's file."),
    ("r", "Check the servers again."),
    ("Enter", "Launch the selected profile."),
    ("?", "Show or hide this help."),
    ("q, Esc, or Ctrl-C", "Quit."),
];

/// Draw the menu, fitting `app.view` and the open help to the screen.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let list_width = list_width(app, body.width);
    let [list_area, details_area] =
        Layout::horizontal([Constraint::Length(list_width), Constraint::Min(10)]).areas(body);

    draw_list(frame, app, list_area);
    draw_details(frame, app, details_area);
    if let Some(help) = &mut app.help {
        draw_help(frame, help, details_area);
    }
    // The footer reads the help's scroll limit, so it comes after the help.
    draw_footer(frame, app, footer);
    if app.effort_picker.is_some() {
        draw_effort_picker(frame, app, body);
    }
}

/// Draw the help over the details pane, beside the list whose marks it explains.
fn draw_help(frame: &mut Frame, help: &mut Scroll, area: Rect) {
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" Help ");
    let text = help_text(block.inner(area).width);
    draw_scrolled(frame, area, block, text, help);
}

/// The help screen for a pane `width` columns wide: what the list's marks and
/// colors mean, and the menu's keys.
fn help_text(width: u16) -> Text<'static> {
    let width = usize::from(width);
    let label_width = 2 + MARKS
        .iter()
        .map(|(state, _)| state.label().width())
        .max()
        .unwrap_or(0);
    let keys_width = 2 + KEYS.iter().map(|(keys, _)| keys.width()).max().unwrap_or(0);

    let mut lines = vec![Line::from("Server marks").bold()];
    for &(state, meaning) in &MARKS {
        let name = vec![
            mark(Some(state)),
            Span::styled(
                format!("{:<label_width$}", state.label()),
                state_style(state),
            ),
        ];
        lines.extend(help_row(name, meaning, width));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::raw("Yellow").fg(Color::Yellow),
        Span::raw(" profile names mark a file error or an undefined provider."),
    ]));
    lines.push(Line::default());
    lines.push(Line::from("Menu keys").bold());
    for &(keys, action) in &KEYS {
        let name = vec![Span::raw(format!("{keys:<keys_width$}")).bold()];
        lines.extend(help_row(name, action, width));
    }
    Text::from(lines)
}

/// A help row: `name`, then `meaning` wrapped within `width` so that it stays
/// in its own column. A column too narrow to read is left to the paragraph.
fn help_row(name: Vec<Span<'static>>, meaning: &str, width: usize) -> Vec<Line<'static>> {
    let indent: usize = name.iter().map(Span::width).sum();
    let column = width.saturating_sub(indent);
    let mut parts = if column < MIN_MEANING_WIDTH {
        vec![meaning.to_owned()]
    } else {
        wrap(meaning, column)
    }
    .into_iter();
    let mut first = name;
    first.extend(parts.next().map(Span::raw));
    std::iter::once(Line::from(first))
        .chain(parts.map(|part| Line::from(format!("{}{part}", " ".repeat(indent)))))
        .collect()
}

/// Break `text` at spaces into lines at most `width` columns wide. A longer
/// word gets a line of its own.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split(' ') {
        match lines.last_mut() {
            Some(line) if line.width() + 1 + word.width() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}

fn draw_effort_picker(frame: &mut Frame, app: &App, body: Rect) {
    let picker = app.effort_picker.as_ref().unwrap();
    let choices = app.effort_choices();
    let height = u16::try_from(choices.levels.len() + 7)
        .unwrap_or(u16::MAX)
        .min(body.height);
    let width = 66.min(body.width);
    let area = Rect::new(
        body.x + (body.width - width) / 2,
        body.y + (body.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(format!(" Effort · {} ", app.selected_profile().name));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [note, list, save] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    let message = match choices.control {
        crate::effort::Control::Unsupported => {
            "No effort controls available for this model.".to_owned()
        }
        crate::effort::Control::Toggle => {
            "Thinking on/off from Ollama; no graded effort levels.".to_owned()
        }
        crate::effort::Control::OffOnly => {
            "This model does not support thinking. Choose off to override.".to_owned()
        }
        _ => match choices.metadata_source {
            Some(source) => format!("Levels from {source}."),
            None => "Model support unknown; these are common levels.".to_owned(),
        },
    };
    frame.render_widget(
        Paragraph::new(message).dim().wrap(Wrap { trim: false }),
        note,
    );
    let items: Vec<ListItem> = std::iter::once(app.inherit_label())
        .chain(choices.levels.iter().map(|level| choices.label(level)))
        .map(ListItem::new)
        .collect();
    let list_widget = List::new(items)
        .highlight_symbol("> ")
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_stateful_widget(list_widget, list, &mut state);
    let name = app
        .selected_profile()
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let actions = if choices.levels.is_empty() {
        "Enter: use inherited setting\nEsc: close".to_owned()
    } else {
        format!("Enter: this launch only\ns: Save to {name}")
    };
    frame.render_widget(Paragraph::new(actions).dim(), save);
}

fn list_width(app: &App, total: u16) -> u16 {
    let longest = app
        .catalog()
        .profiles
        .iter()
        .map(|profile| profile.name.width())
        .max()
        .unwrap_or(0);
    let marks = if app.checks_started() { MARK_WIDTH } else { 0 };
    // Two border columns and the two-column highlight symbol.
    let wanted = u16::try_from(longest + 4)
        .unwrap_or(u16::MAX)
        .saturating_add(marks);
    wanted
        .clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH + marks)
        .min(total.saturating_sub(10).max(MIN_LIST_WIDTH.min(total)))
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let marks = app.checks_started();
    let items: Vec<ListItem> = app
        .catalog()
        .profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let style = if profile.error.is_some() || profile.provider.missing {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            let mut spans = Vec::new();
            if marks {
                spans.push(mark(app.server_status_at(index).map(|status| status.state)));
            }
            spans.push(Span::styled(profile.name.clone(), style));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" Profiles "))
        .highlight_symbol("> ")
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD));
    app.view.list.select(Some(app.selected()));
    frame.render_stateful_widget(list, area, &mut app.view.list);
}

fn draw_details(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(format!(" {} ", app.selected_profile().name));
    let text = details(app);
    draw_scrolled(frame, area, block, text, &mut app.view.details);
}

/// Draw `text` wrapped in `block` at the offset in `scroll`.
///
/// Records the page height and scroll limit in `scroll`, and shows which lines
/// are visible in the bottom border when the text does not fit.
fn draw_scrolled<'a>(
    frame: &mut Frame,
    area: Rect,
    block: Block<'a>,
    text: Text<'a>,
    scroll: &mut Scroll,
) {
    let inner = block.inner(area);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });

    // line_count wraps exactly as rendering does, so the scroll limit matches the screen.
    let lines = u16::try_from(paragraph.line_count(inner.width)).unwrap_or(u16::MAX);
    scroll.fit(inner.height, lines);

    let mut block = block;
    if scroll.max_offset > 0 {
        let shown_to = scroll.offset.saturating_add(inner.height).min(lines);
        block = block.title_bottom(
            Line::from(format!(" {}-{} of {} ", scroll.offset + 1, shown_to, lines))
                .right_aligned(),
        );
    }
    frame.render_widget(paragraph.block(block).scroll((scroll.offset, 0)), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.notice {
        Some(notice) => Line::from(notice.clone()).fg(Color::Yellow),
        None if app.effort_picker.is_some() && app.effort_choices().levels.is_empty() => {
            Line::from("Enter use inherited setting · Esc cancel").dim()
        }
        None if app.effort_picker.is_some() => {
            Line::from("↑/↓ choose · Enter apply · s save · Esc cancel").dim()
        }
        None if app.help.as_ref().is_some_and(|help| help.max_offset > 0) => {
            Line::from("↑/↓ scroll · Esc close").dim()
        }
        None if app.help.is_some() => Line::from("Esc close").dim(),
        None if app.has_effort_override() => hints(OVERRIDE_HINTS, area.width),
        None => hints(HINTS, area.width),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// `rest` after "↑/↓ move", or alone when both do not fit in `width`. The
/// arrows are the first hint to go, as the keys least in need of one.
fn hints(rest: &'static str, width: u16) -> Line<'static> {
    let full = format!("↑/↓ move · {rest}");
    if full.width() <= usize::from(width) {
        Line::from(full).dim()
    } else {
        Line::from(rest).dim()
    }
}

/// The details pane text for the selected profile.
pub fn details(app: &App) -> Text<'static> {
    let profile = app.selected_profile();
    let command = app.command();
    let mut lines = Vec::new();

    if let Some(description) = &profile.description {
        lines.push(Line::from(description.clone()));
        lines.push(Line::default());
    }
    if let Some(error) = &profile.error {
        lines.push(Line::from(format!("Error: {error}")).fg(Color::Yellow));
        lines.push(Line::default());
    }

    lines.push(setting_line(profile, "model", profile.model.as_ref()));
    lines.push(provider_line(profile));
    if let Some(url) = &profile.provider.base_url {
        lines.push(row("url", url.clone(), ""));
    }
    if let Some(status) = app.server_status() {
        lines.push(server_line(status));
    }
    lines.push(setting_line(profile, "effort", app.effective_effort()));
    // Most profiles leave these to Codex, so they show only when a layer sets them.
    for (label, setting) in [
        ("context", &profile.context_window),
        ("compact", &profile.auto_compact_token_limit),
    ] {
        if setting.is_some() {
            lines.push(setting_line(profile, label, setting.as_ref()));
        }
    }
    lines.push(setting_line(profile, "sandbox", profile.sandbox.as_ref()));
    lines.push(setting_line(profile, "approval", profile.approval.as_ref()));
    lines.push(row("launch", command.display(), ""));
    if let Some(wrapper) = &command.wrapper {
        lines.push(row("wrapper", display_path(wrapper, app.home()), ""));
    }

    let file_name = profile
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    lines.push(Line::default());
    match &profile.raw {
        Some(raw) => {
            lines.push(Line::from(format!("── {file_name} ──")).dim());
            lines.extend(raw.lines().map(|line| Line::from(line.to_owned())));
        }
        None => lines.push(Line::from(format!("{file_name} does not exist.")).dim()),
    }
    Text::from(lines)
}

/// The tag that marks where a value came from.
///
/// Every value of the default entry comes from config.toml, so only profile
/// entries mark values that they inherit.
fn source_tag(profile: &Profile, source: Source) -> &'static str {
    match source {
        Source::Profile => "",
        Source::Base if profile.is_default() => "",
        Source::Base => "(base)",
        Source::Builtin => "(built-in)",
        Source::Override => "(override)",
        Source::Arguments => "(arguments)",
    }
}

fn setting_line(profile: &Profile, label: &str, setting: Option<&Setting>) -> Line<'static> {
    match setting {
        Some(setting) => row(
            label,
            setting.value.clone(),
            source_tag(profile, setting.source),
        ),
        None => row(label, "(Codex default)".to_owned(), ""),
    }
}

fn provider_line(profile: &Profile) -> Line<'static> {
    let provider = &profile.provider;
    let value = match &provider.name {
        Some(name) => format!("{} ({name})", provider.id),
        None => provider.id.clone(),
    };
    let mut line = row("provider", value, source_tag(profile, provider.source));
    if provider.missing {
        line.push_span(Span::raw("  not defined").fg(Color::Yellow));
    } else if provider.source == Source::Profile && provider.defined_in == Some(Source::Base) {
        line.push_span(Span::raw("  defined in base").dim());
    }
    line
}

/// A distinct symbol for each state, so the list reads without color.
fn mark(state: Option<State>) -> Span<'static> {
    let Some(state) = state else {
        return Span::raw("  ");
    };
    let symbol = match state {
        State::Ready => "✓ ",
        State::Reachable => "? ",
        State::ModelMissing => "! ",
        State::Down => "✗ ",
        State::Checking => "… ",
        // A dim blank would show as a gray block under the selection's
        // reverse video.
        State::Skipped => return Span::raw("  "),
    };
    Span::styled(symbol, state_style(state))
}

fn server_line(status: &Status) -> Line<'static> {
    let mut text = status.state.label().to_owned();
    if status.state == State::Checking {
        text.push('…');
    }
    if let Some(message) = &status.message {
        text.push_str(": ");
        text.push_str(message);
    }
    Line::from(vec![
        Span::raw(format!("{:<LABEL_WIDTH$}", "server")).bold(),
        Span::styled(text, state_style(status.state)),
    ])
}

fn state_style(state: State) -> Style {
    match state {
        State::Ready => Style::new().fg(Color::Green),
        State::Reachable | State::ModelMissing => Style::new().fg(Color::Yellow),
        State::Down => Style::new().fg(Color::Red),
        State::Checking | State::Skipped => Style::new().add_modifier(Modifier::DIM),
    }
}

fn row(label: &str, value: String, tag: &str) -> Line<'static> {
    let mut spans = vec![
        Span::raw(format!("{label:<LABEL_WIDTH$}")).bold(),
        Span::raw(value),
    ];
    if !tag.is_empty() {
        spans.push(Span::raw(format!("  {tag}")).dim());
    }
    Line::from(spans)
}

/// Show `path` relative to `home` as `~/...` when it is inside it.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Render a buffer as text, one line per row, without trailing spaces.
pub fn buffer_text(buffer: &Buffer) -> String {
    let width = usize::from(buffer.area.width);
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for row in buffer.content.chunks(width) {
        let mut line = String::new();
        let mut skip = 0usize;
        for cell in row {
            if skip == 0 {
                line.push_str(cell.symbol());
            }
            skip = skip.max(cell.symbol().width()).saturating_sub(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Key;
    use crate::test_support::{fixture_app, fixture_home};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn renders_menu_and_selected_details() {
        let (_home, mut app) = fixture_app();
        app.press(Key::Down);
        let screen = render(&mut app, 100, 30);
        assert!(screen.contains("> gateway"), "{screen}");
        assert!(screen.contains("│  default"), "{screen}");
        assert!(
            screen.contains("Team gateway with a key from the environment."),
            "{screen}"
        );
        assert!(screen.contains("model     gpt-team"), "{screen}");
        assert!(screen.contains("effort    high  (base)"), "{screen}");
        assert!(
            screen.contains("launch    codex --profile gateway"),
            "{screen}"
        );
        assert!(screen.contains("── gateway.config.toml ──"), "{screen}");
        assert!(screen.contains("GATEWAY_API_KEY"), "{screen}");
        assert!(!screen.contains("sk-test-secret"), "{screen}");
        assert!(screen.contains("q quit"), "{screen}");
    }

    #[test]
    fn default_profile_shows_base_settings() {
        let (_home, mut app) = fixture_app();
        let screen = render(&mut app, 100, 30);
        assert!(screen.contains("> default"), "{screen}");
        assert!(
            screen.contains(crate::profile::DEFAULT_DESCRIPTION),
            "{screen}"
        );
        assert!(screen.contains("provider  openai  (built-in)"), "{screen}");
        assert!(screen.contains("launch    codex"), "{screen}");
        assert!(!screen.contains("(base)"), "{screen}");
    }

    #[test]
    fn context_and_compaction_show_beside_effort_when_set() {
        let (_home, mut app) = fixture_app();
        app.select_name("local").unwrap();
        let screen = render(&mut app, 100, 30);
        let rows: Vec<&str> = screen.lines().collect();
        let effort = rows
            .iter()
            .position(|row| row.contains("effort    medium"))
            .unwrap_or_else(|| panic!("{screen}"));
        assert!(rows[effort + 1].contains("context   65536"), "{screen}");
        assert!(rows[effort + 2].contains("compact   57344"), "{screen}");

        // A profile that leaves both to Codex shows neither row.
        app.select_name("gateway").unwrap();
        let screen = render(&mut app, 100, 30);
        assert!(!screen.contains("context "), "{screen}");
        assert!(!screen.contains("compact "), "{screen}");

        // Each row depends only on its own key.
        let home = fixture_home();
        std::fs::write(
            home.path().join("compact-only.config.toml"),
            "model_auto_compact_token_limit = 1000\n",
        )
        .unwrap();
        let mut app = App::new(crate::profile::load(home.path()).unwrap(), &[], None, None);
        app.select_name("compact-only").unwrap();
        let screen = render(&mut app, 100, 30);
        assert!(screen.contains("compact   1000"), "{screen}");
        assert!(!screen.contains("context "), "{screen}");
    }

    #[test]
    fn long_details_scroll_and_show_position() {
        let (_home, mut app) = fixture_app();
        app.press(Key::Down);
        let screen = render(&mut app, 80, 12);
        assert!(screen.contains(" of "), "{screen}");
        let max = app.view.details.max_offset;
        assert!(max > 0);
        for _ in 0..20 {
            app.press(Key::PageDown);
        }
        let screen = render(&mut app, 80, 12);
        assert_eq!(app.view.details.offset, max);
        assert!(
            screen.contains("GATEWAY_API_KEY") || screen.contains("http_headers"),
            "{screen}"
        );
    }

    #[test]
    fn narrow_footers_leave_out_the_arrows_first() {
        let footer = |app: &mut App, width: u16| -> String {
            render(app, width, 24).lines().last().unwrap().to_owned()
        };
        let (_home, mut app) = fixture_app();
        assert_eq!(
            footer(&mut app, 83),
            "↑/↓ move · e effort · r recheck · Enter launch · PgUp/PgDn scroll · ? help · q quit"
        );
        assert_eq!(
            footer(&mut app, 82),
            "e effort · r recheck · Enter launch · PgUp/PgDn scroll · ? help · q quit"
        );
        app.set_effort("low").unwrap();
        assert_eq!(
            footer(&mut app, 68),
            "↑/↓ move · e effort · s save effort · Enter launch · ? help · q quit"
        );
        assert_eq!(
            footer(&mut app, 67),
            "e effort · s save effort · Enter launch · ? help · q quit"
        );
    }

    #[test]
    fn help_explains_the_marks_beside_the_list() {
        let (_home, mut app) = fixture_app();
        app.press(Key::Char('?'));
        let screen = render(&mut app, 100, 30);
        for expected in [
            "│> default",
            "┌ Help ",
            "✓ ready          The server lists the profile's model.",
            "✗ down           No answer,",
            "  not checked    The details pane's server line says why.",
            "Yellow profile names",
            "q, Esc, or Ctrl-C  Quit.",
        ] {
            assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
        }
        assert!(screen.ends_with("\nEsc close\n"), "{screen}");
        assert!(!screen.contains("launch    codex"), "{screen}");
    }

    #[test]
    fn help_scrolls_when_it_does_not_fit() {
        let (_home, mut app) = fixture_app();
        app.press(Key::Char('?'));
        let screen = render(&mut app, 80, 12);
        let help = app.help.as_ref().unwrap();
        assert!(help.max_offset > 0);
        let position = format!(" 1-{} of {} ", help.page, help.page + help.max_offset);
        assert!(
            screen.contains(&position),
            "{position:?} missing:\n{screen}"
        );
        assert!(screen.contains("↑/↓ scroll · Esc close"), "{screen}");
        assert!(!screen.contains("Quit."), "{screen}");
        app.press(Key::End);
        let screen = render(&mut app, 80, 12);
        assert!(screen.contains("Quit."), "{screen}");
    }

    #[test]
    fn help_keeps_its_columns_beside_a_wide_list() {
        let home = fixture_home();
        let name = "a-long-profile-name-for-tests";
        std::fs::write(home.path().join(format!("{name}.config.toml")), "").unwrap();
        let mut app = App::new(crate::profile::load(home.path()).unwrap(), &[], None, None);
        app.press(Key::Char('?'));
        let screen = render(&mut app, 80, 40);
        // The widest list leaves the help 46 columns, so meanings wrap within
        // their column instead of back to the border.
        for expected in [
            "│? reachable      The server answered, but not ",
            "│                 with a model list. ",
            "│s                  Save the chosen effort to ",
            "│                   the profile's file. ",
        ] {
            assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
        }
    }

    #[test]
    fn selected_unchecked_profile_has_a_plain_highlight() {
        // Only the default entry, whose built-in OpenAI provider is never checked.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "model = \"gpt-base\"\n").unwrap();
        let mut app = App::new(crate::profile::load(home.path()).unwrap(), &[], None, None);
        app.check_servers();
        assert_eq!(app.server_status_at(0).unwrap().state, State::Skipped);
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // Every cell of the selected row, up to the list's right border, has
        // the highlight's style.
        let row: Vec<_> = (1..buffer.area.width)
            .map(|x| &buffer[(x, 1)])
            .take_while(|cell| cell.symbol() != "│")
            .collect();
        let text: String = row.iter().map(|cell| cell.symbol()).collect();
        assert!(text.starts_with(">   default"), "{text:?}");
        for (column, cell) in row.iter().enumerate() {
            assert_eq!(cell.style(), row[0].style(), "column {column} of {text:?}");
        }
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let (_home, mut app) = fixture_app();
        for (width, height) in [(1, 1), (10, 3), (20, 5), (0, 0)] {
            let _ = render(&mut app, width, height);
            app.press(Key::Char('e'));
            let _ = render(&mut app, width, height);
            app.press(Key::Esc);
            app.press(Key::Char('?'));
            let _ = render(&mut app, width, height);
            app.press(Key::Esc);
        }
    }

    #[test]
    fn home_paths_are_shortened() {
        let home = Path::new("/home/u");
        assert_eq!(
            display_path(Path::new("/home/u/bin/x"), Some(home)),
            "~/bin/x"
        );
        assert_eq!(display_path(Path::new("/opt/x"), Some(home)), "/opt/x");
    }

    #[test]
    fn buffer_text_handles_wide_characters() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        buffer.set_string(0, 0, "a界b", Style::new());
        assert_eq!(buffer_text(&buffer), "a界b\n");
    }
}
