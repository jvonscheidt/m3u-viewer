//! Binary entry point: argument parsing, terminal setup, and the event
//! loop gluing keys, background [`LoadEvent`]s, and the VLC player to the
//! application state.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};
use m3u_viewer::app::App;
use m3u_viewer::loader::{self, LoadEvent};
use m3u_viewer::player::{Player, PlayerError};
use m3u_viewer::store::Store;
use m3u_viewer::ui;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

/// Parsed command line: `m3u-viewer <playlist.m3u> [--vlc <path>]`.
struct Args {
    playlist: PathBuf,
    vlc_override: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut playlist = None;
    let mut vlc_override = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--vlc" {
            let Some(path) = args.next() else {
                bail!("--vlc needs a path argument");
            };
            vlc_override = Some(PathBuf::from(path));
        } else if playlist.is_none() {
            playlist = Some(PathBuf::from(arg));
        } else {
            bail!("unexpected argument: {}", arg.to_string_lossy());
        }
    }
    let Some(playlist) = playlist else {
        bail!("usage: m3u-viewer <playlist.m3u> [--vlc <path-to-vlc>]");
    };
    Ok(Args {
        playlist,
        vlc_override,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if !args.playlist.is_file() {
        bail!("not a readable file: {}", args.playlist.display());
    }
    let file_name = args.playlist.file_name().map_or_else(
        || args.playlist.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    // Discovery failure is not fatal: browsing works without VLC, and the
    // error surfaces in the status bar on the first play attempt.
    let player = Player::discover(args.vlc_override.as_deref());
    let store = Store::default_dir().map(Store::load);
    let events = loader::spawn(args.playlist);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &events, &player, file_name, store);
    ratatui::restore();
    result
}

/// Event loop: drain loader batches, redraw, dispatch key presses, and
/// hand play requests to VLC until the user quits.
fn run(
    terminal: &mut DefaultTerminal,
    events: &Receiver<LoadEvent>,
    player: &Result<Player, PlayerError>,
    file_name: String,
    store: Option<Store>,
) -> Result<()> {
    let mut app = App::new(file_name, store);
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
            if let Some(request) = app.take_play_request() {
                match player.as_ref().map(|p| p.play(&request.url)) {
                    Ok(Ok(())) => {
                        // Confirmation first: a failing recents save then
                        // overrides it with its own error message.
                        app.set_message(format!("▶ {} in VLC", request.name));
                        app.record_played(&request.url);
                    }
                    Ok(Err(error)) => app.set_message(format!("✗ {error}")),
                    Err(error) => app.set_message(format!("✗ {error}")),
                }
            }
        }
    }
}
