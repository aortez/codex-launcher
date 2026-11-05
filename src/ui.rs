//! Drawing the menu and turning a frame into plain text.

use std::path::Path;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::profile::{Profile, Setting, Source};

const LABEL_WIDTH: usize = 10;
const MIN_LIST_WIDTH: u16 = 14;
const MAX_LIST_WIDTH: u16 = 32;
const HELP: &str = "↑/↓ move · e effort · Enter launch · PgUp/PgDn scroll · q quit";

/// Draw the menu and record the list and details pane geometry in `app.view`.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let list_width = list_width(app, body.width);
    let [list_area, details_area] =
        Layout::horizontal([Constraint::Length(list_width), Constraint::Min(10)]).areas(body);

    draw_list(frame, app, list_area);
    draw_details(frame, app, details_area);
    draw_footer(frame, app, footer);
    if app.effort_picker.is_some() {
        draw_effort_picker(frame, app, body);
    }
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
    let message = match choices.metadata_source {
        Some(source) => format!("Levels from {source}."),
        None => "Model support unknown; these are common levels.".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(message).dim().wrap(Wrap { trim: false }),
        note,
    );
    let items: Vec<ListItem> = std::iter::once(app.inherit_label())
        .chain(choices.levels.iter().cloned())
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
    frame.render_widget(
        Paragraph::new(format!("Enter: this launch only\ns: Save to {name}")).dim(),
        save,
    );
}

fn list_width(app: &App, total: u16) -> u16 {
    let longest = app
        .catalog()
        .profiles
        .iter()
        .map(|profile| profile.name.width())
        .max()
        .unwrap_or(0);
    // Two border columns and the two-column highlight symbol.
    let wanted = u16::try_from(longest + 4).unwrap_or(u16::MAX);
    wanted
        .clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH)
        .min(total.saturating_sub(10).max(MIN_LIST_WIDTH.min(total)))
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .catalog()
        .profiles
        .iter()
        .map(|profile| {
            let style = if profile.error.is_some() || profile.provider.missing {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            ListItem::new(Line::styled(profile.name.clone(), style))
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
    let profile = app.selected_profile();
    let title = format!(" {} ", profile.name);
    let text = details(app);
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });

    // line_count wraps exactly as rendering does, so the scroll limit matches the screen.
    let total = u16::try_from(paragraph.line_count(inner.width)).unwrap_or(u16::MAX);
    app.view.details_page = inner.height;
    app.view.details_max_scroll = total.saturating_sub(inner.height);
    app.view.details_scroll = app.view.details_scroll.min(app.view.details_max_scroll);

    let mut block = block;
    if app.view.details_max_scroll > 0 {
        let shown_to = app
            .view
            .details_scroll
            .saturating_add(inner.height)
            .min(total);
        block = block.title_bottom(
            Line::from(format!(
                " {}-{} of {} ",
                app.view.details_scroll + 1,
                shown_to,
                total
            ))
            .right_aligned(),
        );
    }
    frame.render_widget(
        paragraph.block(block).scroll((app.view.details_scroll, 0)),
        area,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.notice {
        Some(notice) => Line::from(notice.clone()).fg(Color::Yellow),
        None if app.effort_picker.is_some() => {
            Line::from("↑/↓ choose · Enter apply · s save · Esc cancel").dim()
        }
        None if app.has_effort_override() => {
            Line::from("↑/↓ move · e effort · s save effort · Enter launch · q quit").dim()
        }
        None => Line::from(HELP).dim(),
    };
    frame.render_widget(Paragraph::new(line), area);
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
    lines.push(setting_line(profile, "effort", app.effective_effort()));
    if profile.context_window.is_some() {
        lines.push(setting_line(
            profile,
            "context",
            profile.context_window.as_ref(),
        ));
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
    use crate::test_support::fixture_app;
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
    fn long_details_scroll_and_show_position() {
        let (_home, mut app) = fixture_app();
        app.press(Key::Down);
        let screen = render(&mut app, 80, 12);
        assert!(screen.contains(" of "), "{screen}");
        let max = app.view.details_max_scroll;
        assert!(max > 0);
        for _ in 0..20 {
            app.press(Key::PageDown);
        }
        let screen = render(&mut app, 80, 12);
        assert_eq!(app.view.details_scroll, max);
        assert!(
            screen.contains("GATEWAY_API_KEY") || screen.contains("http_headers"),
            "{screen}"
        );
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let (_home, mut app) = fixture_app();
        for (width, height) in [(1, 1), (10, 3), (20, 5), (0, 0)] {
            let _ = render(&mut app, width, height);
            app.press(Key::Char('e'));
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
