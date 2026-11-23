//! Drawing the menu and turning a frame into plain text.

use std::path::Path;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Page, Scroll, SessionList};
use crate::check::{State, Status};
use crate::launch::{Command, shell_quote};
use crate::profile::{Profile, Setting, Source};
use crate::sessions::{self, Entry, Match, Session, clean};

const LABEL_WIDTH: usize = 10;
const MIN_LIST_WIDTH: u16 = 14;
const MAX_LIST_WIDTH: u16 = 32;
/// A server mark and the space after it, shown once checks start.
const MARK_WIDTH: u16 = 2;
/// The main menu's key hints, after "↑/↓ move".
const HINTS: &str =
    "e effort · r recheck · Tab sessions · Enter launch · PgUp/PgDn scroll · ? help · q quit";
/// The first hint on every page with a list.
const MOVE_HINT: &str = "↑/↓ move";
/// The hints that a narrow footer leaves out, in order. The arrows go first,
/// as the keys least in need of a hint.
const DROPPED_HINTS: [&str; 2] = [MOVE_HINT, "PgUp/PgDn scroll"];
/// The sessions view's key hints, after "↑/↓ move".
const SESSION_HINTS: &str =
    "Tab profiles · a every directory · Enter resume · PgUp/PgDn scroll · ? help · q quit";
/// The sessions view's key hints while it shows every directory.
const ALL_SESSION_HINTS: &str =
    "Tab profiles · a this directory · Enter resume · PgUp/PgDn scroll · ? help · q quit";
/// The key hints while an effort override is unsaved, after "↑/↓ move".
const OVERRIDE_HINTS: &str = "e effort · s save effort · Enter launch · ? help · q quit";
/// The narrowest help meaning column worth keeping beside its name.
const MIN_MEANING_WIDTH: usize = 20;

/// What each server mark means, in the order the help lists them.
const MARKS: [(State, &str); 9] = [
    (
        State::Ready,
        "The server lists the profile's model, or its status command says ready.",
    ),
    (
        State::StartsOnLaunch,
        "The status command says launching starts the server.",
    ),
    (
        State::Busy,
        "The status command says the server is busy with other work.",
    ),
    (
        State::ModelMissing,
        "The server answered without the model.",
    ),
    (
        State::Reachable,
        "The server answered, but not with a model list.",
    ),
    (
        State::Unknown,
        "The status helper could not determine availability; see the details.",
    ),
    (
        State::Down,
        "No answer, a 5xx error, refused credentials, or a status report saying down.",
    ),
    (
        State::Checking,
        "The server or status command has not answered yet.",
    ),
    (State::Skipped, "The details pane's server line says why."),
];

/// The profiles view's keys, as the help lists them.
const PROFILE_KEYS: [(&str, &str); 10] = [
    ("↑/↓ or k/j", "Move the selection."),
    ("Home/End or g/G", "Select the first or last profile."),
    ("PgUp/PgDn", "Scroll the details."),
    ("Tab", "Show the sessions."),
    ("e", "Choose the reasoning effort."),
    ("s", "Save the chosen effort to the profile's file."),
    ("r", "Check the servers again."),
    ("Enter", "Launch the selected profile."),
    ("?", "Show or hide this help."),
    ("q, Esc, or Ctrl-C", "Quit."),
];

/// The sessions view's keys, as the help lists them.
const SESSION_KEYS: [(&str, &str); 8] = [
    ("↑/↓ or k/j", "Move the selection."),
    ("Home/End or g/G", "Select the first or last session."),
    ("PgUp/PgDn", "Scroll the details."),
    ("Tab", "Show the profiles."),
    ("a", "Show the sessions of every directory, or of this one."),
    ("Enter", "Resume the selected session."),
    ("?", "Show or hide this help."),
    ("q, Esc, or Ctrl-C", "Quit."),
];

/// What the sessions view shows, as the help explains it.
const SESSION_NOTES: [&str; 3] = [
    "Sessions that ran in this directory, or in every directory after a, newest first.",
    "Each resumes with the profile that uses the provider it recorded. When several do, its \
     model narrows them, and Enter asks which of the rest.",
    "Dim names are a git branch or ID, for sessions without a name.",
];

/// The sessions table's headers, with the directory last.
const SESSION_HEADERS: [&str; 5] = ["Name", "Updated", "Model", "Profile", "Directory"];
/// The narrowest that each sessions column shrinks to fit the screen. The
/// last update never shrinks, since a cut one would mislead.
const SESSION_MIN_WIDTHS: [usize; 5] = [12, usize::MAX, 10, 10, 12];
/// The directory, which only the list of every directory shows.
const DIRECTORY_COLUMN: usize = 4;
const SESSION_COLUMN_SPACING: u16 = 2;
const HIGHLIGHT_SYMBOL: &str = "> ";

/// Draw the menu, fitting `app.view` and the open help to the screen.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    match app.page() {
        Page::Profiles => {
            app.view.session_rows = 0..0;
            let list_width = list_width(app, body.width);
            let [list_area, details_area] =
                Layout::horizontal([Constraint::Length(list_width), Constraint::Min(10)])
                    .areas(body);
            draw_list(frame, app, list_area);
            draw_details(frame, app, details_area);
            // The help sits beside the list whose marks it explains.
            if let Some(help) = &mut app.help {
                draw_help(frame, help, details_area, Page::Profiles);
            }
        }
        Page::Sessions => {
            draw_sessions(frame, app, body);
            if let Some(help) = &mut app.help {
                draw_help(frame, help, body, Page::Sessions);
            }
        }
    }
    // The footer reads the help's scroll limit, so it comes after the help.
    draw_footer(frame, app, footer);
    if app.effort_picker.is_some() {
        draw_effort_picker(frame, app, body);
    }
    if app.profile_picker.is_some() {
        draw_profile_picker(frame, app, body);
    }
}

/// Draw the help for `page` over `area`.
fn draw_help(frame: &mut Frame, help: &mut Scroll, area: Rect, page: Page) {
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" Help ");
    let text = help_text(block.inner(area).width, page);
    draw_scrolled(frame, area, block, text, help);
}

/// The help screen for a pane `width` columns wide: what the page's marks and
/// colors mean, and its keys.
fn help_text(width: u16, page: Page) -> Text<'static> {
    let width = usize::from(width);
    let mut lines = Vec::new();
    let keys: &[(&str, &str)] = match page {
        Page::Profiles => {
            let label_width = 2 + MARKS
                .iter()
                .map(|(state, _)| state.label().width())
                .max()
                .unwrap_or(0);
            lines.push(Line::from("Server marks").bold());
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
            &PROFILE_KEYS
        }
        Page::Sessions => {
            lines.push(Line::from("Sessions").bold());
            lines.extend(SESSION_NOTES.iter().map(|&note| Line::from(note)));
            lines.push(Line::from(vec![
                Span::raw("Yellow").fg(Color::Yellow),
                Span::raw(
                    " marks a log that could not be read, a session that no profile can resume, \
                     or another model that resuming would load on a server you run.",
                ),
            ]));
            &SESSION_KEYS
        }
    };
    let keys_width = 2 + keys.iter().map(|(keys, _)| keys.width()).max().unwrap_or(0);
    lines.push(Line::default());
    lines.push(Line::from("Menu keys").bold());
    for &(keys, action) in keys {
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
        crate::effort::Control::Unsupported if app.selected_profile().harness.is_some() => {
            "Effort is configured by this harness preset.".to_owned()
        }
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
        None if app.profile_picker.is_some() => {
            Line::from("↑/↓ choose · Enter resume · Esc cancel").dim()
        }
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
        None if app.page() == Page::Sessions => {
            let all = app.session_list().is_some_and(SessionList::shows_all);
            hints(
                if all {
                    ALL_SESSION_HINTS
                } else {
                    SESSION_HINTS
                },
                area.width,
            )
        }
        None if app.has_effort_override() => hints(OVERRIDE_HINTS, area.width),
        None => hints(HINTS, area.width),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// "↑/↓ move", then `rest`, leaving out hints in the order of DROPPED_HINTS
/// until the line fits in `width`.
fn hints(rest: &str, width: u16) -> Line<'static> {
    let mut shown: Vec<&str> = std::iter::once(MOVE_HINT)
        .chain(rest.split(" · "))
        .collect();
    for dropped in DROPPED_HINTS {
        if shown.join(" · ").width() <= usize::from(width) {
            break;
        }
        shown.retain(|hint| *hint != dropped);
    }
    Line::from(shown.join(" · ")).dim()
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

    if let Some(harness) = &profile.harness {
        lines.push(row("harness", harness.label.clone(), ""));
    }
    lines.push(setting_line(profile, "model", profile.model.as_ref()));
    lines.push(provider_line(profile));
    if let Some(url) = &profile.provider.base_url {
        lines.push(row("url", url.clone(), ""));
    }
    let server = app.server_status();
    if let Some(status) = server {
        lines.push(server_line(status));
    }
    lines.push(setting_line(profile, "effort", app.effective_effort()));
    // Most profiles leave these to Codex, so each shows only when a layer sets
    // it. The context also shows when a status command reports the server's.
    let allocated = server.and_then(|status| status.context_window);
    if profile.context_window.is_some() || allocated.is_some() {
        let mut line = setting_line(profile, "context", profile.context_window.as_ref());
        let configured = profile
            .context_window
            .as_ref()
            .and_then(|setting| setting.value.parse::<u64>().ok());
        if let Some(tokens) = allocated
            && configured != Some(tokens)
        {
            line.push_span(Span::raw(format!("  server allocates {tokens}")).fg(Color::Yellow));
        }
        lines.push(line);
    }
    if profile.auto_compact_token_limit.is_some() {
        lines.push(setting_line(
            profile,
            "compact",
            profile.auto_compact_token_limit.as_ref(),
        ));
    }
    if let Some(drafting) = server.and_then(|status| status.drafting.clone()) {
        lines.push(row("drafting", drafting, ""));
    }
    if let Some(harness) = &profile.harness {
        if let Some(tools) = &harness.tools {
            lines.push(row("tools", tools.clone(), ""));
        }
    } else {
        lines.push(setting_line(profile, "sandbox", profile.sandbox.as_ref()));
        lines.push(setting_line(profile, "approval", profile.approval.as_ref()));
    }
    lines.extend(launch_lines(command, app.home()));
    if let Some(status) = &command.status {
        lines.push(row(
            "status",
            command_line(&status.path, &status.args, app.home()),
            "",
        ));
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

/// The rows that show what launching runs.
fn launch_lines(command: &Command, home: Option<&Path>) -> Vec<Line<'static>> {
    match &command.wrapper {
        // The launch command already names a wrapper from codex-launcher.toml.
        Some(wrapper) if command.configured => {
            vec![row(
                "launch",
                command_line(wrapper, &command.args, home),
                "",
            )]
        }
        wrapper => {
            let mut lines = vec![row("launch", command.display(), "")];
            if let Some(wrapper) = wrapper {
                lines.push(row("wrapper", display_path(wrapper, home), ""));
            }
            lines
        }
    }
}

/// Draw the sessions table, with the selected entry's details below it.
fn draw_sessions(frame: &mut Frame, app: &mut App, area: Rect) {
    app.view.session_rows = 0..0;
    let list = app.session_list();
    let block = sessions_block(app, list);
    let message = match list {
        None => Some("Reading sessions…"),
        Some(list) if list.is_loading() => Some("Reading sessions…"),
        Some(list) if list.row_count() > 0 => None,
        Some(list) if list.shows_all() => Some("No sessions in any directory."),
        Some(_) => Some("No sessions in this directory. Press a to show every directory."),
    };
    let (Some(list), None) = (list, message) else {
        let message = Paragraph::new(message.unwrap_or_default())
            .dim()
            .wrap(Wrap { trim: false })
            .block(block);
        frame.render_widget(message, area);
        return;
    };
    let Some(entry) = list.selected_entry() else {
        return;
    };
    let inner_width = area.width.saturating_sub(2);
    let details = session_details(app, entry, usize::from(inner_width));
    let details_block = Block::bordered().title(format!(" {} ", entry_title(entry)));
    // The table keeps room for its rows, header, and borders, up to half the
    // screen. The details take the rest that they need, and the table any
    // room left over.
    let table_wanted = u16::try_from(list.row_count())
        .unwrap_or(u16::MAX)
        .saturating_add(3);
    let details_wanted = u16::try_from(
        Paragraph::new(details.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner_width),
    )
    .unwrap_or(u16::MAX)
    .saturating_add(2);
    let height = details_wanted.min(area.height - table_wanted.min(area.height / 2));
    let [table_area, details_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).areas(area);
    let table = session_table(app, list, block, table_area.width);
    let selected = list.selected();
    let row_count = list.row_count();
    app.view.sessions.select(Some(selected));
    frame.render_stateful_widget(table, table_area, &mut app.view.sessions);
    // One row per line, below the top border and the header, and above the
    // bottom border.
    let shown = usize::from(table_area.height.saturating_sub(3));
    let first = app.view.sessions.offset();
    app.view.session_rows = first..(first + shown).min(row_count);
    draw_scrolled(
        frame,
        details_area,
        details_block,
        details,
        &mut app.view.session_details,
    );
}

/// The sessions table's border, titled with the sessions it lists, and with
/// any warnings at the bottom.
fn sessions_block(app: &App, list: Option<&SessionList>) -> Block<'static> {
    let named = list.and_then(SessionList::named);
    let place = match (named, list.is_some_and(SessionList::shows_all)) {
        (Some(name), _) => format!("named {:?}", clean(name)),
        (None, true) => "every directory".to_owned(),
        (None, false) => app.current_dir().map_or_else(
            || "this directory".to_owned(),
            |dir| clean(&display_path(dir, app.home())),
        ),
    };
    let mut block = Block::bordered().title(format!(" Sessions · {place} "));
    if let Some(list) = list
        && !list.warnings().is_empty()
    {
        let warnings = clean(&list.warnings().join("; "));
        block = block.title_bottom(Line::from(format!(" {warnings} ")).fg(Color::Yellow));
    }
    block
}

fn entry_title(entry: &Entry) -> String {
    match entry {
        Entry::Session(session) => clean(session.title()),
        Entry::Unknown(_) => "unknown log".to_owned(),
    }
}

/// The sessions table, with each column fitted to `width`.
fn session_table(
    app: &App,
    list: &SessionList,
    block: Block<'static>,
    width: u16,
) -> Table<'static> {
    // Only the list of the current directory's sessions leaves out their
    // directory.
    let columns = if list.shows_all() || list.named().is_some() {
        5
    } else {
        4
    };
    let now = SystemTime::now();
    let rows: Vec<Vec<(String, Style)>> = list
        .rows()
        .map(|entry| {
            let mut cells = session_cells(app, entry, now);
            cells.truncate(columns);
            cells
        })
        .collect();
    let headers = &SESSION_HEADERS[..columns];
    let mut wanted: Vec<usize> = headers.iter().map(|header| header.width()).collect();
    for cells in &rows {
        for (column, (text, _)) in cells.iter().enumerate() {
            wanted[column] = wanted[column].max(text.width());
        }
    }
    let spacing = usize::from(SESSION_COLUMN_SPACING) * (columns - 1);
    // The borders, the highlight symbol, and a margin on the right take room
    // too.
    let available = usize::from(width).saturating_sub(2 + HIGHLIGHT_SYMBOL.width() + spacing + 1);
    let widths = fit_columns(
        wanted,
        &SESSION_MIN_WIDTHS[..columns],
        DIRECTORY_COLUMN,
        available,
    );
    let rows = rows.into_iter().map(|cells| {
        Row::new(
            cells
                .into_iter()
                .enumerate()
                .map(|(column, (text, style))| {
                    // A directory keeps its last components, which tell
                    // directories apart.
                    let text = if column == DIRECTORY_COLUMN {
                        truncate_left(&text, widths[column])
                    } else {
                        truncate(&text, widths[column])
                    };
                    Cell::from(Span::styled(text, style))
                })
                .collect::<Vec<_>>(),
        )
    });
    let constraints = widths
        .iter()
        .map(|&width| Constraint::Length(u16::try_from(width).unwrap_or(u16::MAX)));
    Table::new(rows, constraints)
        .header(Row::new(headers.iter().copied()).bold())
        .column_spacing(SESSION_COLUMN_SPACING)
        .highlight_symbol(HIGHLIGHT_SYMBOL)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .block(block)
}

/// A sessions table row's cells, before fitting: name, last update, model,
/// profile, and directory.
fn session_cells(app: &App, entry: &Entry, now: SystemTime) -> Vec<(String, Style)> {
    let plain = Style::new();
    let yellow = Style::new().fg(Color::Yellow);
    match entry {
        Entry::Session(session) => {
            let matched = app.session_match(session);
            let profile = if matched.profiles.is_empty() {
                ("none".to_owned(), yellow)
            } else {
                (profile_names(app, &matched), plain)
            };
            let name_style = if session.name.is_some() {
                plain
            } else {
                Style::new().add_modifier(Modifier::DIM)
            };
            vec![
                (clean(session.title()), name_style),
                (sessions::ago(session.updated, now), plain),
                (
                    session.model.as_deref().map(clean).unwrap_or_default(),
                    plain,
                ),
                profile,
                (clean(&display_path(&session.cwd, app.home())), plain),
            ]
        }
        Entry::Unknown(unknown) => vec![
            ("unknown".to_owned(), yellow),
            (sessions::ago(unknown.updated, now), plain),
            (String::new(), plain),
            (String::new(), plain),
            (
                unknown
                    .cwd
                    .as_deref()
                    .map(|cwd| clean(&display_path(cwd, app.home())))
                    .unwrap_or_default(),
                plain,
            ),
        ],
    }
}

fn profile_names(app: &App, matched: &Match) -> String {
    matched
        .profiles
        .iter()
        .map(|&index| app.catalog().profiles[index].name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Column widths that fit in `available`: the `wanted` widths, shrunk but
/// none below its minimum. The column `first` gives up its room first, then
/// the widest column. The first column takes any room left over.
fn fit_columns(
    mut widths: Vec<usize>,
    minimum: &[usize],
    first: usize,
    available: usize,
) -> Vec<usize> {
    let mut total: usize = widths.iter().sum();
    if first < widths.len() {
        let spare = widths[first].saturating_sub(minimum[first]);
        let given = spare.min(total.saturating_sub(available));
        widths[first] -= given;
        total -= given;
    }
    while total > available {
        let Some(widest) = (0..widths.len())
            .filter(|&column| widths[column] > minimum[column])
            .max_by_key(|&column| widths[column])
        else {
            break;
        };
        widths[widest] -= 1;
        total -= 1;
    }
    if total < available {
        widths[0] += available - total;
    }
    widths
}

/// `text` cut to `width` columns, ending in an ellipsis when cut.
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut used = 0;
    for c in text.chars() {
        used += c.width().unwrap_or(0);
        if used >= width {
            break;
        }
        kept.push(c);
    }
    if width > 0 {
        kept.push('…');
    }
    kept
}

/// `text` cut to its last `width` columns, starting with an ellipsis when cut.
fn truncate_left(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        used += c.width().unwrap_or(0);
        if used >= width {
            break;
        }
        kept.push(c);
    }
    let ellipsis = (width > 0).then_some('…');
    ellipsis.into_iter().chain(kept.into_iter().rev()).collect()
}

/// The details pane text, `width` columns wide, for a session or a log that
/// could not be read.
fn session_details(app: &App, entry: &Entry, width: usize) -> Text<'static> {
    let now = SystemTime::now();
    let updated = |time| {
        format!(
            "{} · {}",
            sessions::ago(time, now),
            sessions::utc_minute(time)
        )
    };
    let path =
        |name: &str, path: &Path| path_rows(name, &clean(&display_path(path, app.home())), width);
    let mut lines = Vec::new();
    match entry {
        Entry::Unknown(unknown) => {
            lines.extend(path("log", &unknown.path));
            match &unknown.cwd {
                Some(cwd) => lines.extend(path("directory", cwd)),
                None => lines.push(row("directory", "unknown".to_owned(), "")),
            }
            lines.push(row("updated", updated(unknown.updated), ""));
            lines.push(Line::from(vec![
                label("error"),
                Span::raw(clean(&unknown.error)).fg(Color::Yellow),
            ]));
        }
        Entry::Session(session) => {
            let matched = app.session_match(session);
            if let Some(name) = &session.name {
                lines.push(row("name", clean(name), ""));
            }
            lines.push(row("id", session.id.clone(), ""));
            lines.extend(path("directory", &session.cwd));
            if let Some(branch) = &session.branch {
                lines.push(row("branch", clean(branch), ""));
            }
            lines.push(row("updated", updated(session.updated), ""));
            lines.push(row("provider", clean(&session.provider), ""));
            let mut model = match &session.model {
                Some(model) => row("model", clean(model), ""),
                None => row("model", "(not recorded)".to_owned(), ""),
            };
            if matched.model_differs {
                // A hosted provider just answers with the other model, but a
                // server you run may have to load it first.
                let note = Span::raw(format!("  {}", other_model(app, &matched)));
                let loads = matched
                    .profiles
                    .iter()
                    .any(|&index| app.launch_loads_model(index));
                model.push_span(if loads {
                    note.fg(Color::Yellow)
                } else {
                    note.dim()
                });
            }
            lines.push(model);
            lines.push(session_profile_line(app, session, &matched));
            if let &[index] = matched.profiles.as_slice() {
                match app.session_command(session, index) {
                    Some(command) => lines.extend(launch_lines(&command, app.home())),
                    None => lines.push(Line::from(vec![
                        label("launch"),
                        Span::raw(
                            "not with these launch arguments; give none, or a bare \"resume\"",
                        )
                        .fg(Color::Yellow),
                    ])),
                }
            }
            let logs = if session.logs.len() > 1 {
                "logs"
            } else {
                "log"
            };
            for (index, log) in session.logs.iter().enumerate() {
                lines.extend(path(if index == 0 { logs } else { "" }, log));
            }
        }
    }
    Text::from(lines)
}

/// Rows that show `path` beside the label `name` in `width` columns. A long
/// path breaks after a slash where it can, or else after a hyphen or an
/// underscore, and continues under the value. A row never holds only the
/// character that it breaks after.
fn path_rows(name: &str, path: &str, width: usize) -> Vec<Line<'static>> {
    let column = width.saturating_sub(LABEL_WIDTH).max(1);
    let mut parts = Vec::new();
    let mut rest = path;
    loop {
        let mut end = 0;
        let mut used = 0;
        for (index, c) in rest.char_indices() {
            used += c.width().unwrap_or(0);
            if used > column && end > 0 {
                break;
            }
            end = index + c.len_utf8();
        }
        if end >= rest.len() {
            parts.push(rest);
            break;
        }
        let cut = rest[..end]
            .rfind('/')
            .filter(|&index| index > 0)
            .or_else(|| rest[..end].rfind(['-', '_']).filter(|&index| index > 0))
            .map_or(end, |index| index + 1);
        parts.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| row(if index == 0 { name } else { "" }, part.to_owned(), ""))
        .collect()
}

/// What the matching profiles use instead of the session's model.
fn other_model(app: &App, matched: &Match) -> String {
    match matched.profiles.as_slice() {
        &[index] => {
            let profile = &app.catalog().profiles[index];
            match profile.model_name() {
                Some(model) => format!("{} uses {}", profile.name, clean(model)),
                None => format!("{} uses Codex's default model", profile.name),
            }
        }
        _ => "the matching profiles use other models".to_owned(),
    }
}

fn session_profile_line(app: &App, session: &Session, matched: &Match) -> Line<'static> {
    let provider = clean(&session.provider);
    match matched.profiles.as_slice() {
        [] => Line::from(vec![
            label("profile"),
            Span::raw("none").fg(Color::Yellow),
            Span::raw(format!("  needs a profile that uses provider {provider}")).fg(Color::Yellow),
        ]),
        [_] if matched.fallback => {
            let reason = if app.catalog().base_providers.contains(&session.provider) {
                format!("(config.toml defines {provider})")
            } else {
                format!("({provider} is built in)")
            };
            row("profile", profile_names(app, matched), &reason)
        }
        [_] => row("profile", profile_names(app, matched), ""),
        _ => row("profile", profile_names(app, matched), "(Enter asks which)"),
    }
}

/// The popup that asks which profile resumes a session.
fn draw_profile_picker(frame: &mut Frame, app: &App, body: Rect) {
    let picker = app.profile_picker.as_ref().unwrap();
    let profiles = &app.catalog().profiles;
    let name_width = picker
        .profiles
        .iter()
        .map(|&index| profiles[index].name.width())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = picker
        .profiles
        .iter()
        .map(|&index| {
            let profile = &profiles[index];
            let model = profile
                .model_name()
                .map_or_else(|| "(Codex default)".to_owned(), clean);
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<name_width$}  ", profile.name)),
                Span::raw(model).dim(),
            ]))
        })
        .collect();
    let height = u16::try_from(items.len() + 4)
        .unwrap_or(u16::MAX)
        .min(body.height);
    let width = 60.min(body.width);
    let area = Rect::new(
        body.x + (body.width - width) / 2,
        body.y + (body.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" Resume with ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [note, list] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    let title = app
        .picker_session()
        .map(|session| clean(session.title()))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!("Several profiles can resume {title:?}."))
            .dim()
            .wrap(Wrap { trim: false }),
        note,
    );
    let list_widget = List::new(items)
        .highlight_symbol(HIGHLIGHT_SYMBOL)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_stateful_widget(list_widget, list, &mut state);
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
        Some(setting) if profile.harness.is_some() => row(label, setting.value.clone(), "(preset)"),
        Some(setting) => row(
            label,
            setting.value.clone(),
            source_tag(profile, setting.source),
        ),
        // A harness program picks these itself, so the launcher has no value
        // to report and no Codex default to name.
        None if profile.harness.is_some() => row(label, "(set by harness)".to_owned(), ""),
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

/// Symbols make the list readable without color; details explain uncertainty.
fn mark(state: Option<State>) -> Span<'static> {
    let Some(state) = state else {
        return Span::raw("  ");
    };
    let symbol = match state {
        State::Ready => "✓ ",
        // Neither circle has an ambiguous width, so no terminal draws one wide.
        State::StartsOnLaunch => "◌ ",
        State::Busy => "◉ ",
        State::Reachable | State::Unknown => "? ",
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
        State::StartsOnLaunch => Style::new().fg(Color::Cyan),
        State::Busy | State::Reachable | State::Unknown | State::ModelMissing => {
            Style::new().fg(Color::Yellow)
        }
        State::Down => Style::new().fg(Color::Red),
        State::Checking | State::Skipped => Style::new().add_modifier(Modifier::DIM),
    }
}

fn row(name: &str, value: String, tag: &str) -> Line<'static> {
    let mut spans = vec![label(name), Span::raw(value)];
    if !tag.is_empty() {
        spans.push(Span::raw(format!("  {tag}")).dim());
    }
    Line::from(spans)
}

/// A details row's label, padded to line up the values.
fn label(name: &str) -> Span<'static> {
    Span::raw(format!("{name:<LABEL_WIDTH$}")).bold()
}

/// Show `path` relative to `home` as `~/...` when it is inside it.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A command as a shell-quoted line, with a program in the home directory
/// shown as `~/...`, which a shell still expands.
fn command_line(program: &Path, args: &[String], home: Option<&Path>) -> String {
    let program = match home.and_then(|home| program.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", shell_quote(&rest.to_string_lossy())),
        None => shell_quote(&program.to_string_lossy()),
    };
    std::iter::once(program)
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
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
    use crate::test_support::{fixture_app, fixture_home, wait_for_sessions};
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
            footer(&mut app, 98),
            "↑/↓ move · e effort · r recheck · Tab sessions · Enter launch · PgUp/PgDn scroll · ? help · q quit"
        );
        assert_eq!(
            footer(&mut app, 97),
            "e effort · r recheck · Tab sessions · Enter launch · PgUp/PgDn scroll · ? help · q quit"
        );
        assert_eq!(
            footer(&mut app, 86),
            "e effort · r recheck · Tab sessions · Enter launch · ? help · q quit"
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
            "✓ ready             The server lists the profile's model,",
            "◌ starts on launch  The status command says launching starts the server.",
            "◉ busy              The status command says the server is busy with other work.",
            "✗ down              No answer,",
            "  not checked       The details pane's server line says why.",
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
            "│? reachable         The server answered, but ",
            "│                    not with a model list. ",
        ] {
            assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
        }
        app.press(Key::End);
        let screen = render(&mut app, 80, 40);
        for expected in [
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
        let (home, mut app) = fixture_app();
        for (width, height) in [(1, 1), (10, 3), (20, 5), (0, 0)] {
            let _ = render(&mut app, width, height);
            app.press(Key::Char('e'));
            let _ = render(&mut app, width, height);
            app.press(Key::Esc);
            app.press(Key::Char('?'));
            let _ = render(&mut app, width, height);
            app.press(Key::Esc);
        }

        // Two profiles resume the gateway sessions, so Enter opens a picker.
        std::fs::write(
            home.path().join("gateway-backup.config.toml"),
            "model = 'gpt-team'\nmodel_provider = 'gateway'\n\n[model_providers.gateway]\nbase_url = 'https://backup.example.com/v1'\n",
        )
        .unwrap();
        let mut app = App::new(crate::profile::load(home.path()).unwrap(), &[], None, None);
        app.set_current_dir(Path::new("/home/user/project"));
        app.press(Key::Tab);
        for (width, height) in [(1, 1), (10, 3), (20, 5), (0, 0)] {
            let _ = render(&mut app, width, height);
        }
        wait_for_sessions(&mut app);
        for (width, height) in [(1, 1), (10, 3), (20, 5), (0, 0), (30, 8)] {
            for key in [Key::Char('a'), Key::Char('a'), Key::Char('?'), Key::Esc] {
                app.press(key);
                let _ = render(&mut app, width, height);
            }
        }
        let gateway = app
            .session_list()
            .unwrap()
            .rows()
            .position(|entry| {
                entry
                    .session()
                    .is_some_and(|session| session.provider == "gateway")
            })
            .unwrap();
        for _ in 0..gateway {
            app.press(Key::Down);
        }
        app.press(Key::Enter);
        assert!(app.profile_picker.is_some());
        for (width, height) in [(1, 1), (10, 3), (20, 5), (0, 0)] {
            let _ = render(&mut app, width, height);
        }
    }

    #[test]
    fn sessions_show_problems_in_yellow() {
        let (home, mut app) = fixture_app();
        let every_directory = |app: &mut App| {
            app.set_current_dir(Path::new("/home/user/project"));
            app.press(Key::Tab);
            wait_for_sessions(app);
            app.press(Key::Char('a'));
        };
        every_directory(&mut app);
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        // The style of the first cell of `text` in the first line that
        // contains `row`.
        let style_of = |terminal: &Terminal<TestBackend>, row: &str, text: &str| {
            let buffer = terminal.backend().buffer();
            let screen = buffer_text(buffer);
            let (y, line) = screen
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains(row))
                .unwrap_or_else(|| panic!("{row:?} missing:\n{screen}"));
            let x = line[..line.find(text).unwrap()].width();
            buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())].style()
        };
        // Select by ID: two sessions are named "Review the docs", and git keeps
        // no modification times to order them by.
        let select = |app: &mut App, id: &str| {
            let index = app
                .session_list()
                .unwrap()
                .rows()
                .position(|entry| entry.session().is_some_and(|session| session.id == id))
                .unwrap();
            app.press(Key::Home);
            for _ in 0..index {
                app.press(Key::Down);
            }
        };
        let yellow = Some(Color::Yellow);

        // The "Review the docs" session on the gateway, with another model,
        // which the hosted gateway just answers with.
        let review = "019a0000-0000-7000-8000-000000000005";
        select(&mut app, review);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let note = style_of(&terminal, "gateway uses gpt-team", "gateway");
        assert_ne!(note.fg, yellow);
        assert!(note.add_modifier.contains(Modifier::DIM));
        assert_eq!(style_of(&terminal, "│  unknown", "unknown").fg, yellow);
        assert_ne!(
            style_of(&terminal, "│> Review the docs", "Review").fg,
            yellow
        );
        // A session without a name shows its branch, dimmed.
        let branch = style_of(&terminal, "│  docs", "docs");
        assert!(branch.add_modifier.contains(Modifier::DIM));

        // "Lab notes", whose provider no profile uses.
        select(&mut app, "019a0000-0000-7000-8000-000000000006");
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(style_of(&terminal, "│> Lab notes", "none").fg, yellow);
        assert_eq!(
            style_of(&terminal, "none  needs a profile", "none").fg,
            yellow
        );

        // A wrapper may start the gateway profile's own model, and Ollama
        // loads whichever model it is asked for.
        let index = |app: &App, name: &str| app.catalog().index_of(name).unwrap();
        assert!(app.launch_loads_model(index(&app, "local")));
        assert!(!app.launch_loads_model(index(&app, "gateway")));
        std::fs::write(
            home.path().join("codex-launcher.toml"),
            "[[wrapper]]\nprofiles = [\"gateway\"]\nlaunch = [\"/opt/gateway/run\", \"{profile}\"]\n",
        )
        .unwrap();
        let mut app = App::new(crate::profile::load(home.path()).unwrap(), &[], None, None);
        every_directory(&mut app);
        assert!(app.launch_loads_model(index(&app, "gateway")));
        select(&mut app, review);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(
            style_of(&terminal, "gateway uses gpt-team", "gateway").fg,
            yellow
        );
    }

    #[test]
    fn columns_shrink_the_directory_first_then_the_widest() {
        // Name, update, model, profile, and directory.
        let minimum = SESSION_MIN_WIDTHS;
        let fit = |wanted: &[usize], available| {
            fit_columns(wanted.to_vec(), &minimum, DIRECTORY_COLUMN, available)
        };
        assert_eq!(fit(&[20, 6, 8, 7, 30], 100), [49, 6, 8, 7, 30]);
        assert_eq!(fit(&[20, 6, 8, 7, 30], 60), [20, 6, 8, 7, 19]);
        // On a tie, the later column gives way, so the name keeps its room.
        assert_eq!(fit(&[40, 6, 20, 7, 30], 60), [18, 6, 17, 7, 12]);
        // Nothing shrinks below its minimum, even when the screen is too narrow.
        assert_eq!(fit(&[40, 7, 20, 20, 30], 10), [12, 7, 10, 10, 12]);
        assert_eq!(truncate("Fix the flaky tests", 10), "Fix the f…");
        assert_eq!(truncate("界界界", 4), "界…");
        assert_eq!(truncate_left("/home/user/project", 10), "…r/project");
        assert_eq!(truncate("short", 5), "short");
    }

    #[test]
    fn long_paths_break_beside_their_label() {
        let rows = |path: &str, width| -> Vec<String> {
            path_rows("log", path, width)
                .iter()
                .map(|line| line.to_string())
                .collect()
        };
        assert_eq!(
            rows(
                "~/.codex/sessions/2026/11/22/rollout-2026-11-22T09-40-00-019a.jsonl",
                40
            ),
            [
                "log       ~/.codex/sessions/2026/11/22/",
                "          rollout-2026-11-22T09-40-00-",
                "          019a.jsonl",
            ]
        );
        assert_eq!(rows("/work", 40), ["log       /work"]);
        assert_eq!(
            rows("/workspaces-of-mine/x", 20),
            [
                "log       /workspace",
                "          s-of-mine/",
                "          x"
            ]
        );
        assert_eq!(
            rows("abcdefgh", 13),
            ["log       abc", "          def", "          gh"]
        );
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
    fn command_lines_quote_the_program_too() {
        let home = Path::new("/home/u");
        let args = ["work".to_owned(), "two words".to_owned()];
        assert_eq!(
            command_line(Path::new("/home/u/bin/run"), &args, Some(home)),
            "~/bin/run work 'two words'"
        );
        assert_eq!(
            command_line(Path::new("/home/u/my tools/run"), &args, Some(home)),
            "~/'my tools/run' work 'two words'"
        );
        assert_eq!(
            command_line(Path::new("/opt/it's/check"), &[], Some(home)),
            r"'/opt/it'\''s/check'"
        );
    }

    #[test]
    fn buffer_text_handles_wide_characters() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        buffer.set_string(0, 0, "a界b", Style::new());
        assert_eq!(buffer_text(&buffer), "a界b\n");
    }
}
