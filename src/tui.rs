//! The menu loop in a real terminal, and its headless twin for automation.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::crossterm::event::{self, Event};

use crate::app::{Action, App, Instance, Key, Status};
use crate::control::{self, Pending, Request, Server};
use crate::ui;

/// How often the terminal loop checks for control requests between key presses.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Run the menu in this terminal, then launch the chosen profile.
pub fn run_terminal(mut app: App) -> Result<ExitCode> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "the menu needs a terminal; use `codex-launcher list`, \
             `codex-launcher <profile>`, or `--headless`"
        );
    }
    let instance = Instance::current("tui", true);
    let server = match Server::start(&control::runtime_dir()) {
        Ok(server) => Some(server),
        Err(error) => {
            app.notice = Some(format!("Control socket unavailable: {error:#}"));
            None
        }
    };
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            return Err(error).context("cannot set up the terminal");
        }
    };
    let result = terminal_loop(&mut terminal, &mut app, server.as_ref(), &instance);
    ratatui::restore();
    // Remove the socket now: a successful exec never returns to run destructors.
    drop(server);
    result?;
    match app.status() {
        Status::Launching(command) => Err(command.exec()),
        _ => Ok(ExitCode::SUCCESS),
    }
}

fn terminal_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    server: Option<&Server>,
    instance: &Instance,
) -> Result<()> {
    let mut dirty = true;
    while !app.is_done() {
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            dirty = false;
        }
        if let Some(server) = server {
            while let Some(pending) = server.try_next() {
                serve(terminal, app, instance, pending)?;
                if app.is_done() {
                    return Ok(());
                }
            }
        }
        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(event) => {
                    if let Some(key) = Key::from_event(&event) {
                        app.press(key);
                        dirty = true;
                    }
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
    Ok(())
}

/// Run the menu without a terminal, driven only through the control socket.
///
/// The screen is drawn offscreen at `width` by `height`. Launching prints the
/// command line on stdout instead of running it.
pub fn run_headless(mut app: App, width: u16, height: u16) -> Result<ExitCode> {
    let instance = Instance::current("headless", false);
    let server =
        Server::start(&control::runtime_dir()).context("headless mode needs the control socket")?;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| ui::draw(frame, &mut app))?;
    eprintln!(
        "codex-launcher: headless pid {} listening on {}",
        instance.pid,
        server.path().display()
    );
    while !app.is_done() {
        let Some(pending) = server.next() else {
            break;
        };
        serve(&mut terminal, &mut app, &instance, pending)?;
    }
    drop(server);
    if let Status::Launching(command) = app.status() {
        println!("{}", command.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Apply one control request, redraw, and reply with the resulting state.
fn serve<B>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    instance: &Instance,
    pending: Pending,
) -> Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let failure = match &pending.request {
        Request::State | Request::Screen => None,
        Request::Key { keys } => match parse_keys(keys) {
            Ok(keys) => {
                // Redraw after each key, as typing does, so keys such as PgDn
                // use the layout of the profile that an earlier key selected.
                for key in keys {
                    app.press(key);
                    terminal.draw(|frame| ui::draw(frame, app))?;
                }
                None
            }
            Err(error) => Some(error),
        },
        Request::Select { profile } => app.select_name(profile).err(),
        Request::Effort { level } => app.set_effort(level).err(),
        Request::SaveEffort => app.save_effort().err(),
        Request::Launch => {
            app.apply(Action::Launch);
            None
        }
        Request::Quit => {
            app.apply(Action::Quit);
            None
        }
    };
    let frame = terminal.draw(|frame| ui::draw(frame, app))?;
    let response = match (failure, &pending.request) {
        (Some(error), _) => control::error_response(error),
        (None, Request::Screen) => control::screen_response(ui::buffer_text(frame.buffer)),
        (None, _) => control::state_response(&app.state(instance)),
    };
    pending.reply(&response);
    Ok(())
}

/// Parse every key before applying any, so a typo changes nothing.
fn parse_keys(names: &[String]) -> Result<Vec<Key>, String> {
    if names.is_empty() {
        return Err("no keys given".to_owned());
    }
    names.iter().map(|name| Key::parse(name)).collect()
}
