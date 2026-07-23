//! Binary entry point: argument parsing, terminal setup, and the event
//! loop gluing keys, background [`LoadEvent`]s, and the VLC player to the
//! application state.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};
use m3u_viewer::app::App;
use m3u_viewer::config::{Config, XtreamConfig};
use m3u_viewer::epg::{self, EpgEvent, EpgSource};
use m3u_viewer::loader::{self, LoadEvent, Source};
use m3u_viewer::player::{Player, PlayerError};
use m3u_viewer::store::Store;
use m3u_viewer::ui;
use m3u_viewer::xtream::Account;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

const USAGE: &str = "usage: m3u-viewer <playlist.m3u> [--epg <url-or-file>] [--vlc <path>] [--vlc-reuse-instance]\n       \
     m3u-viewer --xtream <server> --username <user> --password <pass> [--epg <url-or-file>] [--user-agent <ua>] [--vlc <path>] [--vlc-reuse-instance] [--save-config]\n       \
     m3u-viewer [--vlc <path>]   (uses saved Xtream credentials from config)\n       \
     m3u-viewer --version";

const VERSION: &str = concat!("m3u-viewer ", env!("CARGO_PKG_VERSION"));

fn version_requested(args: &[OsString]) -> Result<bool> {
    let has_version = args.iter().any(|arg| arg == "--version" || arg == "-V");
    if has_version && args.len() != 1 {
        bail!("--version cannot be combined with other arguments\n{USAGE}");
    }
    Ok(has_version)
}

/// Parsed command line.
struct Args {
    source: Source,
    /// Status-bar caption: file name or `xtream:<host>`.
    display_name: String,
    vlc_override: Option<PathBuf>,
    /// `User-Agent` header for Xtream requests (CLI or config); kept here
    /// so `--save-config` can persist it.
    user_agent: Option<String>,
    /// Explicit XMLTV guide source (CLI or config); when set it beats a
    /// `url-tvg` header and the Xtream account's own `xmltv.php`.
    epg: Option<String>,
    /// Whether to hand playback requests to a single running VLC instance
    /// (CLI or config; CLI can only turn it on, not override config off).
    vlc_reuse_instance: bool,
    /// When true, persist the resolved credentials + VLC path to the config
    /// file before starting.
    save_config: bool,
}

/// Whether `arg` is one of the recognised option flags — used to detect a
/// flag whose value was omitted (e.g. `--username --password`), so the next
/// flag is not silently swallowed as that value.
fn looks_like_flag(arg: &OsStr) -> bool {
    matches!(
        arg.to_str(),
        Some(
            "--vlc"
                | "--xtream"
                | "--username"
                | "--password"
                | "--user-agent"
                | "--epg"
                | "--vlc-reuse-instance"
                | "--save-config"
                | "--version"
                | "-V"
        )
    )
}

/// Parses CLI arguments, filling in missing Xtream credentials and the VLC
/// path from `config` when they are not provided on the command line.
/// Raw option values collected from the command line, before config
/// fallbacks are applied and the source is resolved.
#[derive(Default)]
struct CliFlags {
    playlist: Option<PathBuf>,
    vlc_override: Option<PathBuf>,
    server: Option<String>,
    username: Option<String>,
    password: Option<String>,
    user_agent: Option<String>,
    epg: Option<String>,
    vlc_reuse_instance: bool,
    save_config: bool,
}

impl CliFlags {
    fn collect(args: impl Iterator<Item = OsString>) -> Result<Self> {
        let mut flags = Self::default();
        let mut args = args;
        while let Some(arg) = args.next() {
            let mut string_flag = |name: &str| -> Result<String> {
                match args.next() {
                    Some(value) if looks_like_flag(&value) => {
                        bail!("{name} needs a value\n{USAGE}")
                    }
                    Some(value) => Ok(value.to_string_lossy().into_owned()),
                    None => bail!("{name} needs a value\n{USAGE}"),
                }
            };
            if arg == "--vlc" {
                flags.vlc_override = Some(PathBuf::from(string_flag("--vlc")?));
            } else if arg == "--xtream" {
                flags.server = Some(string_flag("--xtream")?);
            } else if arg == "--username" {
                flags.username = Some(string_flag("--username")?);
            } else if arg == "--password" {
                flags.password = Some(string_flag("--password")?);
            } else if arg == "--user-agent" {
                flags.user_agent = Some(string_flag("--user-agent")?);
            } else if arg == "--epg" {
                flags.epg = Some(string_flag("--epg")?);
            } else if arg == "--vlc-reuse-instance" {
                flags.vlc_reuse_instance = true;
            } else if arg == "--save-config" {
                flags.save_config = true;
            } else if flags.playlist.is_none() && flags.server.is_none() {
                flags.playlist = Some(PathBuf::from(arg));
            } else {
                bail!("unexpected argument: {}\n{USAGE}", arg.to_string_lossy());
            }
        }
        Ok(flags)
    }

    /// Fills fields the user did not supply from `config`; CLI values
    /// always win.
    fn fill_from_config(&mut self, config: &Config) {
        // Xtream credentials only when --xtream was not given on the CLI
        // and no playlist file was provided either.
        if self.server.is_none()
            && self.playlist.is_none()
            && let Some(ref xtream_cfg) = config.xtream
        {
            self.server = Some(xtream_cfg.server.clone());
            if self.username.is_none() {
                self.username = Some(xtream_cfg.username.clone());
            }
            if self.password.is_none() {
                self.password = Some(xtream_cfg.password.clone());
            }
        }
        if self.vlc_override.is_none() {
            self.vlc_override.clone_from(&config.vlc_path);
        }
        if self.user_agent.is_none() {
            self.user_agent.clone_from(&config.user_agent);
        }
        if self.epg.is_none() {
            self.epg.clone_from(&config.epg_url);
        }
        // A plain boolean flag can't express "off", so it only ever adds
        // to what config already enabled.
        self.vlc_reuse_instance |= config.vlc_reuse_instance;
    }
}

fn parse_args(args: impl Iterator<Item = OsString>, config: &Config) -> Result<Args> {
    let mut flags = CliFlags::collect(args)?;
    flags.fill_from_config(config);
    let CliFlags {
        playlist,
        vlc_override,
        server,
        username,
        password,
        user_agent,
        epg,
        vlc_reuse_instance,
        save_config,
    } = flags;

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
                user_agent,
                epg,
                vlc_reuse_instance,
                save_config,
            })
        }
        (None, Some(server)) => {
            let (Some(username), Some(password)) = (username, password) else {
                bail!("--xtream needs --username and --password\n{USAGE}");
            };
            let account =
                Account::new(&server, username, password).with_user_agent(user_agent.clone());
            let display_name = account.display_name();
            Ok(Args {
                source: Source::Xtream(account),
                display_name,
                vlc_override,
                user_agent,
                epg,
                vlc_reuse_instance,
                save_config,
            })
        }
        (None, None) => bail!("{USAGE}"),
    }
}

/// Initialises file-only logging to `path`, truncating any previous run's
/// log. Silently does nothing if `path` is `None` or the file cannot be
/// created.
fn init_logger(path: Option<&Path>) {
    let Some(path) = path else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(file) = std::fs::File::create(path) {
        let _ = simplelog::WriteLogger::init(
            simplelog::LevelFilter::Info,
            simplelog::Config::default(),
            file,
        );
    }
}

fn main() -> Result<()> {
    let raw_args: Vec<_> = std::env::args_os().skip(1).collect();
    if version_requested(&raw_args)? {
        println!("{VERSION}");
        return Ok(());
    }

    let config_path = Config::default_path();
    let log_path = config_path
        .as_ref()
        .map(|p| p.with_file_name("m3u-viewer.log"));
    init_logger(log_path.as_deref());
    log::info!("m3u-viewer {} starting", env!("CARGO_PKG_VERSION"));

    let config = if let Some(ref path) = config_path {
        match Config::load(path) {
            Ok(cfg) => {
                // load() returns the default when the file is absent; don't
                // log that as if credentials had been read.
                if path.exists() {
                    log::info!("config loaded from: {}", path.display());
                } else {
                    log::info!("no config file at: {}", path.display());
                }
                cfg
            }
            Err(e) => {
                log::warn!("config load error: {e}");
                eprintln!("warning: could not load config: {e}");
                Config::default()
            }
        }
    } else {
        Config::default()
    };

    let args = parse_args(raw_args.into_iter(), &config)?;

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
            user_agent: args.user_agent.clone(),
            epg_url: args.epg.clone(),
            regex_filter: config.regex_filter,
            vlc_reuse_instance: args.vlc_reuse_instance,
        };
        match config_path {
            Some(ref path) => {
                if let Err(e) = new_config.save(path) {
                    log::warn!("config save error: {e}");
                    eprintln!("warning: {e}");
                } else {
                    log::info!("config saved to: {}", path.display());
                }
            }
            None => eprintln!("warning: --save-config: no config directory on this platform"),
        }
    }

    // Discovery failure is not fatal: browsing works without VLC, and the
    // error surfaces in the status bar on the first play attempt.
    let player = Player::discover(args.vlc_override.as_deref())
        .map(|player| player.with_reuse_instance(args.vlc_reuse_instance));
    let store = Store::default_dir().map(Store::load);
    // An explicit --epg/config source wins; an Xtream account brings its
    // own guide endpoint. Plain files without either may still name one
    // in their #EXTM3U header — handled inside the event loop, where the
    // loader reports it as LoadEvent::EpgUrl.
    let epg_source = args.epg.as_deref().map(EpgSource::from_arg).or_else(|| {
        if let Source::Xtream(account) = &args.source {
            Some(EpgSource::Url(account.xmltv_url()))
        } else {
            None
        }
    });
    let epg_runtime = EpgRuntime {
        rx: epg_source.map(|source| epg::spawn(source, args.user_agent.clone())),
        user_agent: args.user_agent,
    };
    let events = loader::spawn(args.source, Store::default_dir());

    let mut terminal = ratatui::init();
    let result = run(
        &mut terminal,
        &events,
        &player,
        args.display_name,
        store,
        config.regex_filter,
        epg_runtime,
    );
    ratatui::restore();
    result
}

/// EPG wiring owned by the event loop: the in-flight guide load, if one
/// started at launch, plus what spawning one later (when the playlist
/// header names a guide URL) needs.
struct EpgRuntime {
    rx: Option<Receiver<EpgEvent>>,
    user_agent: Option<String>,
}

/// Event loop: drain loader batches, redraw, dispatch key presses, and
/// hand play requests to VLC until the user quits.
fn run(
    terminal: &mut DefaultTerminal,
    events: &Receiver<LoadEvent>,
    player: &Result<Player, PlayerError>,
    display_name: String,
    store: Option<Store>,
    regex_filter: bool,
    mut epg_runtime: EpgRuntime,
) -> Result<()> {
    let mut app = App::new(display_name, store);
    app.set_regex_filter(regex_filter);
    if epg_runtime.rx.is_some() {
        app.set_epg_loading();
    }
    loop {
        while let Ok(event) = events.try_recv() {
            // A guide URL discovered in the playlist header starts an EPG
            // load, unless one is already running (explicit --epg/config
            // source, Xtream default, or the same URL from the cached
            // copy of this playlist).
            if let LoadEvent::EpgUrl(url) = &event
                && epg_runtime.rx.is_none()
            {
                epg_runtime.rx = Some(epg::spawn(
                    EpgSource::from_arg(url),
                    epg_runtime.user_agent.clone(),
                ));
                app.set_epg_loading();
            }
            app.on_load_event(event);
        }
        if let Some(rx) = &epg_runtime.rx {
            while let Ok(event) = rx.try_recv() {
                app.on_epg_event(event);
            }
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
            ..Config::default()
        };
        let args = parse_args(std::iter::empty(), &config).unwrap();
        assert!(matches!(args.source, Source::Xtream(_)));
        assert_eq!(args.display_name, "xtream:example.com");
    }

    #[test]
    fn config_vlc_path_fallback() {
        let config = Config {
            vlc_path: Some(PathBuf::from("/usr/bin/vlc")),
            ..Config::default()
        };
        let args = parse_args(["list.m3u"].iter().map(OsString::from), &config).unwrap();
        assert_eq!(args.vlc_override, Some(PathBuf::from("/usr/bin/vlc")));
    }

    #[test]
    fn cli_vlc_overrides_config() {
        let config = Config {
            vlc_path: Some(PathBuf::from("/usr/bin/vlc")),
            ..Config::default()
        };
        let args = parse_args(
            ["list.m3u", "--vlc", "/opt/vlc"].iter().map(OsString::from),
            &config,
        )
        .unwrap();
        assert_eq!(args.vlc_override, Some(PathBuf::from("/opt/vlc")));
    }

    #[test]
    fn user_agent_flag_parsed() {
        let args = parse(&[
            "--xtream",
            "example.com",
            "--username",
            "u",
            "--password",
            "p",
            "--user-agent",
            "VLC/3.0.20",
        ])
        .unwrap();
        assert_eq!(args.user_agent, Some("VLC/3.0.20".to_owned()));
    }

    #[test]
    fn config_user_agent_fallback_and_cli_override() {
        let config = Config {
            user_agent: Some("FromConfig/1.0".to_owned()),
            ..Config::default()
        };
        let args = parse_args(["list.m3u"].iter().map(OsString::from), &config).unwrap();
        assert_eq!(args.user_agent, Some("FromConfig/1.0".to_owned()));

        let args = parse_args(
            ["list.m3u", "--user-agent", "FromCli/2.0"]
                .iter()
                .map(OsString::from),
            &config,
        )
        .unwrap();
        assert_eq!(args.user_agent, Some("FromCli/2.0".to_owned()));
    }

    #[test]
    fn epg_flag_parsed() {
        let args = parse(&["list.m3u", "--epg", "http://example.com/epg.xml.gz"]).unwrap();
        assert_eq!(args.epg, Some("http://example.com/epg.xml.gz".to_owned()));
    }

    #[test]
    fn config_epg_fallback_and_cli_override() {
        let config = Config {
            epg_url: Some("http://config/epg.xml".to_owned()),
            ..Config::default()
        };
        let args = parse_args(["list.m3u"].iter().map(OsString::from), &config).unwrap();
        assert_eq!(args.epg, Some("http://config/epg.xml".to_owned()));

        let args = parse_args(
            ["list.m3u", "--epg", "local-guide.xml"]
                .iter()
                .map(OsString::from),
            &config,
        )
        .unwrap();
        assert_eq!(args.epg, Some("local-guide.xml".to_owned()));
    }

    #[test]
    fn vlc_reuse_instance_flag_parsed() {
        let args = parse(&["list.m3u", "--vlc-reuse-instance"]).unwrap();
        assert!(args.vlc_reuse_instance);
    }

    #[test]
    fn vlc_reuse_instance_falls_back_to_config_and_cli_cannot_disable_it() {
        let config = Config {
            vlc_reuse_instance: true,
            ..Config::default()
        };
        let args = parse_args(["list.m3u"].iter().map(OsString::from), &config).unwrap();
        assert!(args.vlc_reuse_instance);
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

    #[test]
    fn cli_credentials_override_config() {
        // Regression: partial CLI credentials must win over stored ones
        // rather than being silently replaced by the whole config block.
        let config = Config {
            xtream: Some(XtreamConfig {
                server: "http://example.com".to_owned(),
                username: "stored".to_owned(),
                password: "stored-pw".to_owned(),
            }),
            ..Config::default()
        };
        let args = parse_args(
            ["--username", "cli-user"].iter().map(OsString::from),
            &config,
        )
        .unwrap();
        let Source::Xtream(account) = args.source else {
            panic!("expected an Xtream source");
        };
        let (server, username, password) = account.credentials();
        assert_eq!(server, "http://example.com"); // filled from config
        assert_eq!(username, "cli-user"); // CLI wins
        assert_eq!(password, "stored-pw"); // filled from config
    }

    #[test]
    fn missing_flag_value_is_an_error_not_a_swallowed_flag() {
        // `--username` with no value must not consume `--password` as its
        // value.
        let error = parse(&["--xtream", "example.com", "--username", "--password", "p"])
            .err()
            .unwrap();
        assert!(error.to_string().contains("--username needs a value"));
    }

    #[test]
    fn version_flags_are_recognized_without_a_source() {
        assert!(version_requested(&[OsString::from("--version")]).unwrap());
        assert!(version_requested(&[OsString::from("-V")]).unwrap());
    }

    #[test]
    fn version_flag_cannot_be_combined_with_other_arguments() {
        let error = version_requested(&[OsString::from("--version"), OsString::from("list.m3u")])
            .unwrap_err();
        assert!(error.to_string().contains("cannot be combined"));
    }
}
