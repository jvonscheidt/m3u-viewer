//! Binary entry point: argument parsing, terminal setup, and the event
//! loop gluing keys, background [`LoadEvent`]s, and the VLC player to the
//! application state.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};
use m3u_viewer::app::App;
use m3u_viewer::config::{Config, XtreamConfig};
use m3u_viewer::loader::{self, LoadEvent, Source};
use m3u_viewer::player::{Player, PlayerError};
use m3u_viewer::store::Store;
use m3u_viewer::ui;
use m3u_viewer::xtream::Account;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

const USAGE: &str = "usage: m3u-viewer <playlist.m3u> [--vlc <path>]\n       \
     m3u-viewer --xtream <server> --username <user> --password <pass> [--vlc <path>] [--save-config]\n       \
     m3u-viewer [--vlc <path>]   (uses saved Xtream credentials from config)";

/// Parsed command line.
struct Args {
    source: Source,
    /// Status-bar caption: file name or `xtream:<host>`.
    display_name: String,
    vlc_override: Option<PathBuf>,
    /// When true, persist the resolved credentials + VLC path to the config
    /// file before starting.
    save_config: bool,
}

/// Parses CLI arguments, filling in missing Xtream credentials and the VLC
/// path from `config` when they are not provided on the command line.
fn parse_args(args: impl Iterator<Item = OsString>, config: &Config) -> Result<Args> {
    let mut playlist: Option<PathBuf> = None;
    let mut vlc_override = None;
    let mut server: Option<String> = None;
    let mut username: Option<String> = None;
    let mut password: Option<String> = None;
    let mut save_config = false;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut string_flag = |name: &str| -> Result<String> {
            match args.next() {
                Some(value) => Ok(value.to_string_lossy().into_owned()),
                None => bail!("{name} needs a value\n{USAGE}"),
            }
        };
        if arg == "--vlc" {
            vlc_override = Some(PathBuf::from(string_flag("--vlc")?));
        } else if arg == "--xtream" {
            server = Some(string_flag("--xtream")?);
        } else if arg == "--username" {
            username = Some(string_flag("--username")?);
        } else if arg == "--password" {
            password = Some(string_flag("--password")?);
        } else if arg == "--save-config" {
            save_config = true;
        } else if playlist.is_none() && server.is_none() {
            playlist = Some(PathBuf::from(arg));
        } else {
            bail!("unexpected argument: {}\n{USAGE}", arg.to_string_lossy());
        }
    }

    // Fill in Xtream credentials from config when --xtream was not given on
    // the CLI and no playlist file was provided either.
    if server.is_none()
        && playlist.is_none()
        && let Some(ref xtream_cfg) = config.xtream
    {
        server = Some(xtream_cfg.server.clone());
        username = Some(xtream_cfg.username.clone());
        password = Some(xtream_cfg.password.clone());
    }
    // VLC path from config only when --vlc was not given on the CLI.
    if vlc_override.is_none() {
        vlc_override.clone_from(&config.vlc_path);
    }

    match (playlist, server) {
        (Some(_), Some(_)) => bail!("give either a playlist file or --xtream, not both\n{USAGE}"),
        (Some(path), None) => {
            if username.is_some() || password.is_some() {
                bail!("--username/--password only apply to --xtream\n{USAGE}");
            }
            let display_name = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            Ok(Args {
                source: Source::File(path),
                display_name,
                vlc_override,
                save_config,
            })
        }
        (None, Some(server)) => {
            let (Some(username), Some(password)) = (username, password) else {
                bail!("--xtream needs --username and --password\n{USAGE}");
            };
            let account = Account::new(&server, username, password);
            let display_name = account.display_name();
            Ok(Args {
                source: Source::Xtream(account),
                display_name,
                vlc_override,
                save_config,
            })
        }
        (None, None) => bail!("{USAGE}"),
    }
}

fn main() -> Result<()> {
    let config_path = Config::default_path();
    let config = if let Some(ref path) = config_path {
        match Config::load(path) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("warning: could not load config: {e}");
                Config::default()
            }
        }
    } else {
        Config::default()
    };

    let args = parse_args(std::env::args_os().skip(1), &config)?;

    if let Source::File(path) = &args.source
        && !path.is_file()
    {
        bail!("not a readable file: {}", path.display());
    }

    if args.save_config {
        let xtream = match &args.source {
            Source::Xtream(account) => {
                let (server, username, password) = account.credentials();
                Some(XtreamConfig {
                    server: server.to_owned(),
                    username: username.to_owned(),
                    password: password.to_owned(),
                })
            }
            // Preserve existing Xtream config when saving with a file source.
            Source::File(_) => config.xtream,
        };
        let new_config = Config {
            xtream,
            vlc_path: args.vlc_override.clone(),
        };
        match config_path {
            Some(ref path) => {
                if let Err(e) = new_config.save(path) {
                    eprintln!("warning: {e}");
                }
            }
            None => eprintln!("warning: --save-config: no config directory on this platform"),
        }
    }

    // Discovery failure is not fatal: browsing works without VLC, and the
    // error surfaces in the status bar on the first play attempt.
    let player = Player::discover(args.vlc_override.as_deref());
    let store = Store::default_dir().map(Store::load);
    let events = loader::spawn(args.source);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &events, &player, args.display_name, store);
    ratatui::restore();
    result
}

/// Event loop: drain loader batches, redraw, dispatch key presses, and
/// hand play requests to VLC until the user quits.
fn run(
    terminal: &mut DefaultTerminal,
    events: &Receiver<LoadEvent>,
    player: &Result<Player, PlayerError>,
    display_name: String,
    store: Option<Store>,
) -> Result<()> {
    let mut app = App::new(display_name, store);
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

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use m3u_viewer::config::XtreamConfig;

    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        parse_args(args.iter().map(OsString::from), &Config::default())
    }

    #[test]
    fn file_source_with_vlc_override() {
        let args = parse(&["list.m3u", "--vlc", "C:/tools/vlc.exe"]).unwrap();
        assert!(matches!(args.source, Source::File(_)));
        assert_eq!(args.display_name, "list.m3u");
        assert_eq!(args.vlc_override, Some(PathBuf::from("C:/tools/vlc.exe")));
    }

    #[test]
    fn xtream_source_needs_full_credentials() {
        let error = parse(&["--xtream", "example.com", "--username", "u"])
            .err()
            .unwrap();
        assert!(error.to_string().contains("--password"));
    }

    #[test]
    fn xtream_source_parses() {
        let args = parse(&[
            "--xtream",
            "example.com",
            "--username",
            "u",
            "--password",
            "p",
        ])
        .unwrap();
        assert!(matches!(args.source, Source::Xtream(_)));
        assert_eq!(args.display_name, "xtream:example.com");
    }

    #[test]
    fn file_and_xtream_are_mutually_exclusive() {
        let error = parse(&["list.m3u", "--xtream", "example.com"])
            .err()
            .unwrap();
        assert!(error.to_string().contains("not both"));
    }

    #[test]
    fn no_source_prints_usage() {
        let error = parse(&[]).err().unwrap();
        assert!(error.to_string().contains("usage:"));
    }

    #[test]
    fn config_xtream_fallback_when_no_cli_source() {
        let config = Config {
            xtream: Some(XtreamConfig {
                server: "http://example.com".to_owned(),
                username: "u".to_owned(),
                password: "p".to_owned(),
            }),
            vlc_path: None,
        };
        let args = parse_args(std::iter::empty(), &config).unwrap();
        assert!(matches!(args.source, Source::Xtream(_)));
        assert_eq!(args.display_name, "xtream:example.com");
    }

    #[test]
    fn config_vlc_path_fallback() {
        let config = Config {
            xtream: None,
            vlc_path: Some(PathBuf::from("/usr/bin/vlc")),
        };
        let args = parse_args(["list.m3u"].iter().map(OsString::from), &config).unwrap();
        assert_eq!(args.vlc_override, Some(PathBuf::from("/usr/bin/vlc")));
    }

    #[test]
    fn cli_vlc_overrides_config() {
        let config = Config {
            xtream: None,
            vlc_path: Some(PathBuf::from("/usr/bin/vlc")),
        };
        let args = parse_args(
            ["list.m3u", "--vlc", "/opt/vlc"].iter().map(OsString::from),
            &config,
        )
        .unwrap();
        assert_eq!(args.vlc_override, Some(PathBuf::from("/opt/vlc")));
    }

    #[test]
    fn save_config_flag_parsed() {
        let args = parse(&[
            "--xtream",
            "example.com",
            "--username",
            "u",
            "--password",
            "p",
            "--save-config",
        ])
        .unwrap();
        assert!(args.save_config);
    }
}
