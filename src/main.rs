//! Binary entry point: terminal setup and the event loop gluing keys and
//! background [`LoadEvent`]s to the application state.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};
use m3u_viewer::app::App;
use m3u_viewer::loader::{self, LoadEvent};
use m3u_viewer::ui;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

fn main() -> Result<()> {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        bail!("usage: m3u-viewer <playlist.m3u>");
    };
    if !path.is_file() {
        bail!("not a readable file: {}", path.display());
    }
    let file_name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let events = loader::spawn(path);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &events, file_name);
    ratatui::restore();
    result
}

/// Event loop: drain loader batches, redraw, and dispatch key presses
/// until the user quits.
fn run(
    terminal: &mut DefaultTerminal,
    events: &Receiver<LoadEvent>,
    file_name: String,
) -> Result<()> {
    let mut app = App::new(file_name);
    loop {
        while let Ok(event) = events.try_recv() {
            app.on_load_event(event);
        }
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        if app.should_quit() {
            return Ok(());
        }
        // Short poll so loader batches keep painting while idle.
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            // Windows delivers Release events too; act on Press only.
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key);
        }
    }
}
