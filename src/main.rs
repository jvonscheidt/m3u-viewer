//! Binary entry point: argument parsing, terminal setup, and the event
//! loop gluing keys, background [`LoadEvent`]s, and the VLC player to the
//! application state.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{NaiveDate, Utc};
use m3u_viewer::app::App;
use m3u_viewer::config::{Config, ConfigError, XtreamConfig};
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
            && let Some(xtream_cfg) = config.xtream()
        {
            self.server = Some(xtream_cfg.server().to_owned());
            if self.username.is_none() {
                self.username = Some(xtream_cfg.username().to_owned());
            }
            if self.password.is_none() {
                self.password = Some(xtream_cfg.password().to_owned());
            }
        }
        if self.vlc_override.is_none() {
            self.vlc_override = config.vlc_path().map(Path::to_path_buf);
        }
        if self.user_agent.is_none() {
            self.user_agent = config.user_agent().map(str::to_owned);
        }
        if self.epg.is_none() {
            self.epg = config.epg_url().map(str::to_owned);
        }
        // A plain boolean flag can't express "off", so it only ever adds
        // to what config already enabled.
        self.vlc_reuse_instance |= config.vlc_reuse_instance();
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

/// How many days a log file collects entries before it is rotated aside.
const LOG_RETENTION_DAYS: i64 = 30;

/// Initialises file-only logging to `path`, appending so a launch no
/// longer discards the previous run, and rotating the file aside once it
/// spans [`LOG_RETENTION_DAYS`]. A missing platform log path disables
/// logging.
///
/// # Errors
///
/// When the log file cannot be opened or the logger registered. Callers
/// treat this as a warning: the viewer works without a log.
fn init_logger(path: Option<&Path>) -> Result<()> {
    let Some(path) = path else { return Ok(()) };
    let (file, rotation_failure) = open_log_file(path, Utc::now().date_naive())?;
    simplelog::WriteLogger::init(
        simplelog::LevelFilter::Info,
        // Dated timestamps: a log spanning up to 30 days needs them to be
        // readable, and the first line's date is what dates the file.
        simplelog::ConfigBuilder::new()
            .set_time_format_rfc3339()
            .build(),
        file,
    )
    .map_err(|_| anyhow!("could not register file logger"))?;
    if let Some(reason) = rotation_failure {
        log::warn!("log rotation skipped, appending instead: {reason}");
    }
    Ok(())
}

/// Opens the log at `path` for appending, rotating it aside first when it
/// is stale.
///
/// A failed rotation — on Windows typically `m3u-viewer.log.old` held open
/// by an antivirus scanner or a log viewer — is not an error: the current
/// file is appended to instead, and the reason is returned so it can be
/// logged once the logger is up.
///
/// # Errors
///
/// When the log directory cannot be created or the file not opened.
fn open_log_file(path: &Path, today: NaiveDate) -> Result<(std::fs::File, Option<String>)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create log directory {}", parent.display()))?;
    }
    let rotation_failure = rotate_if_stale(path, today)
        .err()
        .map(|error| format!("{error:#}"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("could not open log file {}", path.display()))?;
    Ok((file, rotation_failure))
}

/// Date of the oldest entry in a log file, read from the RFC 3339
/// timestamp opening its first line.
///
/// `None` when the file is empty, unreadable, or was written by a version
/// that timestamped entries with the time alone — all of which mean the
/// file's age is unknown and it is due for rotation.
fn log_start_date(path: &Path) -> Option<NaiveDate> {
    let mut first_line = String::new();
    BufReader::new(std::fs::File::open(path).ok()?)
        .read_line(&mut first_line)
        .ok()?;
    NaiveDate::parse_from_str(first_line.get(..10)?, "%Y-%m-%d").ok()
}

/// Renames the log to `<name>.old` once its oldest entry is
/// [`LOG_RETENTION_DAYS`] or more behind `today`, so the previous period
/// is still on disk. Any earlier `.old` file is replaced, keeping the two
/// files bounded.
fn rotate_if_stale(path: &Path, today: NaiveDate) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let within_retention =
        log_start_date(path).is_some_and(|start| (today - start).num_days() < LOG_RETENTION_DAYS);
    if within_retention {
        return Ok(());
    }
    let rotated = path.with_extension("log.old");
    std::fs::rename(path, &rotated).with_context(|| {
        format!(
            "could not rotate log {} to {}",
            path.display(),
            rotated.display()
        )
    })?;
    Ok(())
}

/// Loads favorites and recents, disabling persistence rather than risking
/// overwriting an unreadable store.
fn load_store() -> Option<Store> {
    match Store::default_dir().map(Store::load).transpose() {
        Ok(store) => store,
        Err(error) => {
            log::warn!("persistent store disabled: {error}");
            eprintln!("warning: persistent favorites/recents disabled: {error}");
            None
        }
    }
}

/// The config read at startup, plus why the file on disk could not be
/// used, if it could not.
struct StartupConfig {
    config: Config,
    /// Set when the config file exists but could not be read or parsed.
    /// Startup then runs on defaults, and the file must never be
    /// overwritten: it may hold the only copy of saved credentials.
    unreadable: Option<UnreadableConfig>,
}

/// A config file that exists but failed to load.
struct UnreadableConfig {
    path: PathBuf,
    error: ConfigError,
}

impl UnreadableConfig {
    /// One-line status-bar notice; the full (often multi-line) parse error
    /// goes to stderr and the log instead.
    fn notice(&self) -> String {
        format!(
            "⚠ could not load {} — using defaults (see log)",
            self.path.display()
        )
    }
}

/// Loads the config from `path`, falling back to defaults — but
/// remembering the failure — when the file exists and is unreadable.
fn load_config(path: Option<&Path>) -> StartupConfig {
    let Some(path) = path else {
        return StartupConfig {
            config: Config::default(),
            unreadable: None,
        };
    };
    match Config::load(path) {
        Ok(config) => {
            // load() returns the default when the file is absent; don't
            // log that as if credentials had been read.
            if path.exists() {
                log::info!("config loaded from: {}", path.display());
            } else {
                log::info!("no config file at: {}", path.display());
            }
            StartupConfig {
                config,
                unreadable: None,
            }
        }
        Err(error) => {
            log::warn!(
                "config load error, using defaults ({}): {error}",
                path.display()
            );
            StartupConfig {
                config: Config::default(),
                unreadable: Some(UnreadableConfig {
                    path: path.to_path_buf(),
                    error,
                }),
            }
        }
    }
}

/// Handles `--save-config`: writes the resolved settings to `path`.
///
/// # Errors
///
/// Refuses — before the TUI starts, so the message stays readable — when
/// the existing config file failed to load. Startup is then running on
/// defaults, so saving would silently replace the stored Xtream account
/// and every hand-edited setting with nothing.
fn save_config(args: &Args, startup: &StartupConfig, path: Option<&Path>) -> Result<()> {
    if let Some(unreadable) = &startup.unreadable {
        bail!(
            "--save-config: refusing to overwrite {}, which could not be loaded:\n{}\n\
             Fix the file or delete it, then run again.",
            unreadable.path.display(),
            unreadable.error
        );
    }
    let Some(path) = path else {
        eprintln!("warning: --save-config: no config directory on this platform");
        return Ok(());
    };
    match config_to_save(args, &startup.config).save(path) {
        Ok(()) => log::info!("config saved to: {}", path.display()),
        Err(e) => {
            log::warn!("config save error: {e}");
            eprintln!("warning: {e}");
        }
    }
    Ok(())
}

fn config_to_save(args: &Args, current: &Config) -> Config {
    let xtream = match &args.source {
        Source::Xtream(account) => {
            let (server, username, password) = account.credentials();
            Some(XtreamConfig::new(
                server.to_owned(),
                username.to_owned(),
                password.to_owned(),
            ))
        }
        // Preserve existing Xtream config when saving with a file source.
        Source::File(_) => current.xtream().cloned(),
    };
    Config::default()
        .with_xtream(xtream)
        .with_vlc_path(args.vlc_override.clone())
        .with_user_agent(args.user_agent.clone())
        .with_epg_url(args.epg.clone())
        .with_regex_filter(current.regex_filter())
        .with_vlc_reuse_instance(args.vlc_reuse_instance)
        // No CLI flag sets this, so it can only come from the file.
        .with_vlc_prewarm(current.vlc_prewarm())
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
    // Startup warnings that would otherwise only reach stderr, which the
    // alternate screen hides until the user quits; shown in the status bar.
    let mut notices = Vec::new();
    // The log is a diagnostic aid; failing to open it must never stop
    // the viewer from starting.
    if let Err(error) = init_logger(log_path.as_deref()) {
        eprintln!("warning: logging disabled: {error:#}");
        notices.push("⚠ logging disabled: could not open the log file".to_owned());
    }
    log::info!("m3u-viewer {} starting", env!("CARGO_PKG_VERSION"));

    let startup = load_config(config_path.as_deref());
    if let Some(unreadable) = &startup.unreadable {
        eprintln!(
            "warning: could not load config {}: {}",
            unreadable.path.display(),
            unreadable.error
        );
        notices.push(unreadable.notice());
    }

    let args = parse_args(raw_args.into_iter(), &startup.config)?;

    if let Source::File(path) = &args.source
        && !path.is_file()
    {
        bail!("not a readable file: {}", path.display());
    }

    if args.save_config {
        save_config(&args, &startup, config_path.as_deref())?;
    }
    let config = startup.config;

    // Discovery failure is not fatal: browsing works without VLC, and the
    // error surfaces in the status bar on the first play attempt.
    let player = Player::discover(args.vlc_override.as_deref())
        .map(|player| player.with_reuse_instance(args.vlc_reuse_instance));
    // Start the plugin reads now, so they overlap with loading the
    // playlist rather than with the user's first Enter.
    if config.vlc_prewarm()
        && let Ok(player) = &player
    {
        player.prewarm();
    }
    let store = load_store();
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
    let epg_runtime = EpgRuntime::new(epg_source, args.user_agent);
    let events = loader::spawn(args.source, Store::default_dir());

    let mut app = App::new(args.display_name, store);
    app.set_regex_filter(config.regex_filter());
    if !notices.is_empty() {
        app.set_message(notices.join(" · "));
    }

    let mut terminal = start_terminal(ratatui::try_init, || {
        // Undo whatever part of the setup succeeded (raw mode may be on
        // even though the alternate screen failed); with no console at
        // all this fails too, and there is nothing left to restore.
        let _ = ratatui::try_restore();
    })?;
    let _restore_on_exit = TerminalGuard;
    // After try_init, so the hook it wraps is ratatui's restoring one.
    install_panic_hook();
    run(
        &mut terminal,
        app,
        LoaderFeed::new(events),
        &player,
        epg_runtime,
    )
}

/// Restores the terminal when dropped, so every way out of the TUI — a
/// normal return, an error propagated with `?`, or an unwinding panic —
/// leaves the console usable.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Puts the terminal into TUI mode via `init`, turning a failure — no
/// usable console, e.g. redirected stdin or some IDE/mintty hosts — into
/// an error after running `restore`, instead of the panic
/// `ratatui::init` raises.
fn start_terminal<T>(
    init: impl FnOnce() -> std::io::Result<T>,
    restore: impl FnOnce(),
) -> Result<T> {
    init().map_err(|error| {
        restore();
        anyhow!(error).context(
            "could not set up the terminal; m3u-viewer needs an interactive \
             console (run it directly in a terminal, without redirecting \
             stdin or stdout)",
        )
    })
}

/// Status shown when the playlist loader thread dies without reporting.
const LOADER_DIED: &str = "playlist loader stopped unexpectedly — see log";

/// Failure recorded when the EPG thread dies without reporting.
const EPG_DIED: &str = "EPG loader stopped unexpectedly — see log";

/// The loader's event channel, plus whether its final event has arrived.
struct LoaderFeed {
    rx: Receiver<LoadEvent>,
    /// Set once [`LoadEvent::Finished`] or [`LoadEvent::Failed`] arrived
    /// (or was synthesised): the sender hanging up after that is the
    /// normal end of loading, not a crash.
    done: bool,
}

impl LoaderFeed {
    fn new(rx: Receiver<LoadEvent>) -> Self {
        Self { rx, done: false }
    }

    /// Next queued loader event, if any.
    ///
    /// A disconnect before the final event means the loader thread died
    /// (panicked); it is turned into a [`LoadEvent::Failed`] so the UI
    /// reports it instead of showing "loading" forever.
    fn try_next(&mut self) -> Option<LoadEvent> {
        if self.done {
            return None;
        }
        match self.rx.try_recv() {
            Ok(event) => {
                self.done = matches!(event, LoadEvent::Finished | LoadEvent::Failed(_));
                Some(event)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                log::error!("playlist loader thread ended without reporting a result");
                Some(LoadEvent::Failed(LOADER_DIED.to_owned()))
            }
        }
    }
}

/// Wraps the current panic hook — ratatui's, which restores the terminal
/// and then prints — so it only runs for panics on the UI thread (the
/// caller's). Every panic is logged with its thread name first.
///
/// ratatui's hook fires for a panic on *any* thread, so a dying loader or
/// EPG thread used to drop the terminal out of raw mode and the alternate
/// screen while the UI kept drawing. A background panic now only reaches
/// the log; the event loop notices the dead thread through its closed
/// channel and says so in the status bar.
fn install_panic_hook() {
    let ui_thread = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        log::error!("thread '{name}' {info}");
        if forwards_panic(thread.id(), ui_thread) {
            previous(info);
        }
    }));
}

/// Whether a panic on `panicking` goes on to the terminal-restoring hook:
/// only for the UI thread, which owns the terminal.
fn forwards_panic(panicking: ThreadId, ui_thread: ThreadId) -> bool {
    panicking == ui_thread
}

/// EPG wiring owned by the event loop: the in-flight guide load, if one
/// started at launch, plus what spawning one later (when the playlist
/// header names a guide URL) needs.
struct EpgRuntime {
    rx: Option<Receiver<EpgEvent>>,
    user_agent: Option<String>,
    active_playlist_url: Option<String>,
    pending_playlist_url: Option<String>,
    resolved: bool,
}

impl EpgRuntime {
    fn new(source: Option<EpgSource>, user_agent: Option<String>) -> Self {
        Self {
            rx: source.map(|source| epg::spawn(source, user_agent.clone())),
            user_agent,
            active_playlist_url: None,
            pending_playlist_url: None,
            resolved: false,
        }
    }

    fn observe_playlist_url(&mut self, url: &str) -> bool {
        if self.resolved {
            return false;
        }
        if self.rx.is_some() {
            if self.active_playlist_url.as_deref() != Some(url) {
                self.pending_playlist_url = Some(url.to_owned());
            }
            return false;
        }
        self.start_playlist_url(url.to_owned());
        true
    }

    fn take_event(&mut self) -> Option<EpgEvent> {
        let result = self.rx.as_ref()?.try_recv();
        match result {
            Ok(event) => {
                self.rx = None;
                self.active_playlist_url = None;
                self.resolved = matches!(&event, EpgEvent::Loaded(_));
                if self.resolved {
                    self.pending_playlist_url = None;
                }
                Some(event)
            }
            Err(TryRecvError::Empty) => None,
            // The thread always sends exactly one event before hanging up,
            // so a bare disconnect means it died (panicked). Report it as
            // a failure rather than leaving "epg…" up forever; a pending
            // playlist guide URL may still be tried afterwards.
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.active_playlist_url = None;
                log::error!("EPG thread ended without reporting a result");
                Some(EpgEvent::Failed(EPG_DIED.to_owned()))
            }
        }
    }

    fn start_pending(&mut self) -> bool {
        if self.resolved || self.rx.is_some() {
            return false;
        }
        let Some(url) = self.pending_playlist_url.take() else {
            return false;
        };
        self.start_playlist_url(url);
        true
    }

    fn start_playlist_url(&mut self, url: String) {
        self.rx = Some(epg::spawn(
            EpgSource::from_arg(&url),
            self.user_agent.clone(),
        ));
        self.active_playlist_url = Some(url);
    }
}

/// How long to wait for a key press before looping, so loader batches
/// keep painting while the user is idle.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Repaint at least this often even when nothing changed. The EPG columns
/// render against the current time, so they would otherwise sit frozen
/// until the next key press.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Whether the UI needs painting: either the state changed, or the
/// time-dependent EPG columns are due a refresh.
///
/// Without this the loop repainted on every [`POLL_INTERVAL`] tick — 20
/// full renders a second of an unchanged screen, which costs real CPU
/// once a large playlist and a guide are loaded.
fn needs_redraw(dirty: bool, since_last_draw: Duration) -> bool {
    dirty || since_last_draw >= REFRESH_INTERVAL
}

/// Longest the event loop spends folding loader events into the app
/// before it repaints and polls input again.
///
/// Absorbing a batch costs more the bigger the list already is, while the
/// loader parses at a steady pace; once absorbing fell behind, an
/// unbounded drain never emptied the channel until loading ended — no
/// paint, no key handling, no quit for the whole load.
const DRAIN_BUDGET: Duration = Duration::from_millis(25);

/// Most loader events applied per frame, whatever the clock says.
const MAX_EVENTS_PER_FRAME: usize = 64;

/// What one bounded pass over the background channels did.
#[derive(Debug, PartialEq, Eq)]
struct Drained {
    /// Whether the app state changed, i.e. the screen needs a repaint.
    changed: bool,
    /// Whether the pass stopped at its budget with loader events possibly
    /// still queued, so the loop should come straight back for them.
    backlog: bool,
}

/// Applies what the loader and EPG threads sent since the last call
/// (including their unexpected death).
///
/// Loader events are applied until `deadline` passes or
/// [`MAX_EVENTS_PER_FRAME`] were taken — always at least one, so loading
/// progresses however slow a single absorb is; the rest stay queued for
/// the next frame.
fn apply_background_events(
    app: &mut App,
    loader: &mut LoaderFeed,
    epg_runtime: &mut EpgRuntime,
    deadline: Instant,
) -> Drained {
    let mut changed = false;
    let mut applied = 0;
    let backlog = loop {
        if applied > 0 && (applied >= MAX_EVENTS_PER_FRAME || Instant::now() >= deadline) {
            break true;
        }
        let Some(event) = loader.try_next() else {
            break false;
        };
        // A guide URL discovered in the playlist header starts an EPG
        // load, unless one is already running (explicit --epg/config
        // source, Xtream default, or the same URL from the cached copy of
        // this playlist).
        if let LoadEvent::EpgUrl(url) = &event
            && epg_runtime.observe_playlist_url(url)
        {
            app.set_epg_loading();
        }
        app.on_load_event(event);
        changed = true;
        applied += 1;
    };
    while let Some(event) = epg_runtime.take_event() {
        app.on_epg_event(event);
        changed = true;
    }
    if epg_runtime.start_pending() {
        app.set_epg_loading();
        changed = true;
    }
    Drained { changed, backlog }
}

/// Event loop: drain loader batches, redraw, dispatch key presses, and
/// hand play requests to VLC until the user quits.
fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut loader: LoaderFeed,
    player: &Result<Player, PlayerError>,
    mut epg_runtime: EpgRuntime,
) -> Result<()> {
    if epg_runtime.rx.is_some() {
        app.set_epg_loading();
    }
    let mut dirty = true;
    let mut last_draw = Instant::now();
    loop {
        // Checked first, so a quit pressed mid-load does not wait for
        // another drain and paint.
        if app.should_quit() {
            return Ok(());
        }
        let drained = apply_background_events(
            &mut app,
            &mut loader,
            &mut epg_runtime,
            Instant::now() + DRAIN_BUDGET,
        );
        dirty |= drained.changed;
        if needs_redraw(dirty, last_draw.elapsed()) {
            app.update_viewports(usize::from(terminal.size()?.height));
            terminal.draw(|frame| ui::draw(frame, &app))?;
            dirty = false;
            last_draw = Instant::now();
        }
        // Short poll so loader batches are picked up promptly; no wait at
        // all while a backlog is queued, only a check for pending input.
        let timeout = if drained.backlog {
            Duration::ZERO
        } else {
            POLL_INTERVAL
        };
        if event::poll(timeout)? {
            match event::read()? {
                // Windows delivers Release events too; act on Press only.
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    dirty = true;
                    app.handle_key(key);
                    if let Some(request) = app.take_play_request() {
                        match player.as_ref().map(|p| p.play(request.url())) {
                            Ok(Ok(())) => {
                                // Confirmation first: a failing recents
                                // save then overrides it with its own
                                // error message.
                                app.set_message(format!("▶ {} in VLC", request.name()));
                                app.record_played(request.url());
                            }
                            Ok(Err(error)) => app.set_message(format!("✗ {error}")),
                            Err(error) => app.set_message(format!("✗ {error}")),
                        }
                    }
                }
                // Same state, new layout: repaint without any app change.
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use m3u_viewer::config::XtreamConfig;
    use m3u_viewer::epg::Guide;

    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        parse_args(args.iter().map(OsString::from), &Config::default())
    }

    /// Unique temp dir holding a log file with `contents`.
    fn log_dir_with(tag: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "m3u-viewer-log-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m3u-viewer.log"), contents).unwrap();
        dir
    }

    #[test]
    fn idle_screen_is_not_repainted_every_poll() {
        // The bug: the loop painted once per POLL_INTERVAL regardless of
        // whether anything had changed, burning CPU on a static screen.
        assert!(!needs_redraw(false, POLL_INTERVAL));
        assert!(!needs_redraw(false, Duration::from_millis(950)));
    }

    #[test]
    fn state_changes_paint_immediately() {
        assert!(needs_redraw(true, Duration::ZERO));
    }

    #[test]
    fn idle_screen_still_refreshes_for_the_clock() {
        // EPG columns render against "now", so an untouched screen must
        // keep repainting slowly rather than freezing.
        assert!(needs_redraw(false, REFRESH_INTERVAL));
        assert!(needs_redraw(false, REFRESH_INTERVAL * 3));
    }

    /// Renders `app` the way the event loop does and returns the screen.
    fn screen(app: &mut App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 8)).unwrap();
        app.update_viewports(8);
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn empty_batch() -> LoadEvent {
        LoadEvent::Batch {
            channels: Vec::new(),
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(10),
        }
    }

    fn idle_epg() -> EpgRuntime {
        EpgRuntime::new(None, None)
    }

    /// A drain deadline that never interferes with a test.
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    /// A loader feed with `batches` empty batches and then `Finished`
    /// queued; the sender is kept alive like a still-running loader.
    fn queued_feed(batches: usize) -> (std::sync::mpsc::Sender<LoadEvent>, LoaderFeed) {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..batches {
            tx.send(empty_batch()).unwrap();
        }
        tx.send(LoadEvent::Finished).unwrap();
        (tx, LoaderFeed::new(rx))
    }

    /// Events still queued in `loader`, consuming them.
    fn remaining(loader: &mut LoaderFeed) -> usize {
        std::iter::from_fn(|| loader.try_next()).count()
    }

    #[test]
    fn drain_stops_at_the_event_cap_and_keeps_the_rest_queued() {
        // Regression: the loop drained every queued event before painting,
        // so a backlog that grew faster than it was absorbed froze the UI
        // (no paint, no quit) until loading ended.
        let (_tx, mut loader) = queued_feed(200);
        let mut app = App::new("list.m3u".to_owned(), None);
        let drained = apply_background_events(&mut app, &mut loader, &mut idle_epg(), later());
        assert_eq!(
            drained,
            Drained {
                changed: true,
                backlog: true
            }
        );
        assert_eq!(remaining(&mut loader), 201 - MAX_EVENTS_PER_FRAME);
    }

    #[test]
    fn drain_past_its_deadline_still_applies_one_event() {
        // However slow one absorb is, every frame makes progress.
        let (_tx, mut loader) = queued_feed(10);
        let mut app = App::new("list.m3u".to_owned(), None);
        let drained =
            apply_background_events(&mut app, &mut loader, &mut idle_epg(), Instant::now());
        assert!(drained.changed);
        assert!(drained.backlog);
        assert_eq!(remaining(&mut loader), 10);
    }

    #[test]
    fn bounded_drains_finish_the_load_and_then_go_quiet() {
        let (_tx, mut loader) = queued_feed(3 * MAX_EVENTS_PER_FRAME);
        let mut app = App::new("list.m3u".to_owned(), None);
        let mut epg_runtime = idle_epg();
        let mut passes = 0;
        while apply_background_events(&mut app, &mut loader, &mut epg_runtime, later()).backlog {
            passes += 1;
        }
        assert_eq!(passes, 3);
        assert!(!screen(&mut app).contains("loading"));
        // Nothing new: no repaint is requested, and no backlog either.
        assert_eq!(
            apply_background_events(&mut app, &mut loader, &mut epg_runtime, later()),
            Drained {
                changed: false,
                backlog: false
            }
        );
    }

    #[test]
    fn loader_panic_is_reported_instead_of_loading_forever() {
        // Regression: `while let Ok(..) = try_recv()` treated a dead
        // loader (Disconnected) like an idle one (Empty), so a panic in
        // the loader thread left the status bar on "loading" forever.
        let (tx, rx) = std::sync::mpsc::channel();
        let loader_thread = std::thread::spawn(move || {
            tx.send(empty_batch()).unwrap();
            panic!("simulated loader crash");
        });
        assert!(loader_thread.join().is_err());

        let mut app = App::new("list.m3u".to_owned(), None);
        let mut loader = LoaderFeed::new(rx);
        let drained = apply_background_events(&mut app, &mut loader, &mut idle_epg(), later());
        assert!(drained.changed);
        let screen = screen(&mut app);
        assert!(screen.contains(LOADER_DIED), "{screen}");
        assert!(!screen.contains("loading"), "{screen}");
        // Reported once, not on every frame.
        assert!(loader.try_next().is_none());
    }

    #[test]
    fn hang_up_after_finished_is_the_normal_end_not_a_failure() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(LoadEvent::Finished).unwrap();
        drop(tx);
        let mut loader = LoaderFeed::new(rx);
        assert!(matches!(loader.try_next(), Some(LoadEvent::Finished)));
        assert!(loader.try_next().is_none());
        assert!(loader.try_next().is_none());
    }

    #[test]
    fn idle_loader_is_not_reported_as_dead() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut loader = LoaderFeed::new(rx);
        assert!(loader.try_next().is_none());
        assert!(!loader.done);
    }

    #[test]
    fn epg_thread_death_is_reported_as_a_failed_guide() {
        // Regression: a dead EPG thread was silently forgotten, leaving
        // "epg…" in the status bar for good.
        let (tx, rx) = std::sync::mpsc::channel::<EpgEvent>();
        drop(tx);
        let mut epg_runtime = EpgRuntime {
            rx: Some(rx),
            user_agent: None,
            active_playlist_url: None,
            pending_playlist_url: None,
            resolved: false,
        };
        let mut app = App::new("list.m3u".to_owned(), None);
        app.set_epg_loading();
        let (_loader_tx, loader_rx) = std::sync::mpsc::channel();
        let mut loader = LoaderFeed::new(loader_rx);

        let drained = apply_background_events(&mut app, &mut loader, &mut epg_runtime, later());
        assert!(drained.changed);
        assert!(epg_runtime.rx.is_none());
        assert!(!epg_runtime.resolved, "a fallback guide may still be tried");
        let screen = screen(&mut app);
        assert!(screen.contains("epg ✗"), "{screen}");
        assert!(!screen.contains("epg…"), "{screen}");
    }

    #[test]
    fn only_ui_thread_panics_reach_the_terminal_restoring_hook() {
        let ui_thread = std::thread::current().id();
        assert!(forwards_panic(ui_thread, ui_thread));
        let background = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        assert!(!forwards_panic(background, ui_thread));
    }

    #[test]
    fn terminal_setup_failure_is_an_error_not_a_panic() {
        // Regression: ratatui::init() panicked when there was no usable
        // console (redirected stdin, some IDE hosts).
        let restored = std::cell::Cell::new(false);
        let result = start_terminal(
            || Err::<(), _>(std::io::Error::other("no console")),
            || restored.set(true),
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("interactive console"), "{message}");
        assert!(message.contains("no console"), "keeps the cause: {message}");
        assert!(restored.get(), "partial setup must be undone");
    }

    #[test]
    fn successful_terminal_setup_is_not_restored() {
        let restored = std::cell::Cell::new(false);
        let value = start_terminal(|| Ok(7), || restored.set(true)).unwrap();
        assert_eq!(value, 7);
        assert!(!restored.get());
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn log_within_retention_is_left_alone() {
        let dir = log_dir_with("fresh", "2026-09-01T10:00:00Z [INFO] m3u-viewer starting\n");
        let log = dir.join("m3u-viewer.log");
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        assert!(log.exists());
        assert!(!dir.join("m3u-viewer.log.old").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn log_is_rotated_once_it_spans_the_retention_period() {
        let dir = log_dir_with("stale", "2026-08-01T10:00:00Z [INFO] m3u-viewer starting\n");
        let log = dir.join("m3u-viewer.log");
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        assert!(!log.exists(), "stale log should have been moved aside");
        let rotated = std::fs::read_to_string(dir.join("m3u-viewer.log.old")).unwrap();
        assert!(rotated.contains("2026-08-01"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_boundary_is_thirty_days() {
        let dir = log_dir_with("boundary", "2026-08-13T10:00:00Z [INFO] starting\n");
        let log = dir.join("m3u-viewer.log");
        // Exactly 30 days old: rotated. 29 would be kept.
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        assert!(!log.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn undated_legacy_log_is_rotated() {
        // Logs written before timestamps carried a date cannot be aged,
        // so they are rotated rather than appended to forever.
        let dir = log_dir_with("legacy", "12:21:31 [INFO] m3u-viewer 0.8.0 starting\n");
        let log = dir.join("m3u-viewer.log");
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        assert!(!log.exists());
        assert!(dir.join("m3u-viewer.log.old").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_log_is_rotated_and_missing_log_is_a_no_op() {
        let dir = log_dir_with("empty", "");
        let log = dir.join("m3u-viewer.log");
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        assert!(!log.exists());
        // Second pass: nothing to rotate, and no error.
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotating_twice_replaces_the_previous_archive() {
        let dir = log_dir_with("replace", "2026-07-01T10:00:00Z [INFO] first\n");
        let log = dir.join("m3u-viewer.log");
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        std::fs::write(&log, "2026-08-01T10:00:00Z [INFO] second\n").unwrap();
        rotate_if_stale(&log, date("2026-09-12")).unwrap();
        let rotated = std::fs::read_to_string(dir.join("m3u-viewer.log.old")).unwrap();
        assert!(rotated.contains("second"), "newest archive should win");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_rotation_appends_to_the_current_log_instead_of_failing() {
        // Regression: a rotation failure (on Windows, `.log.old` held open
        // by antivirus or a viewer) aborted startup. A non-empty directory
        // in the archive's place makes the rename fail on every platform.
        let dir = log_dir_with("rotate-fails", "2026-07-01T10:00:00Z [INFO] old run\n");
        let log = dir.join("m3u-viewer.log");
        let blocker = dir.join("m3u-viewer.log.old");
        std::fs::create_dir_all(&blocker).unwrap();
        std::fs::write(blocker.join("keep"), "x").unwrap();

        let (mut file, rotation_failure) = open_log_file(&log, date("2026-09-12")).unwrap();
        let reason = rotation_failure.expect("the failed rotation is reported");
        assert!(reason.contains("could not rotate log"), "{reason}");

        std::io::Write::write_all(&mut file, b"2026-09-12T08:00:00Z [INFO] new run\n").unwrap();
        drop(file);
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(contents.contains("old run"), "previous entries are kept");
        assert!(contents.contains("new run"), "new entries are appended");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn log_file_opens_after_a_successful_rotation() {
        let dir = log_dir_with("rotate-ok", "2026-07-01T10:00:00Z [INFO] old run\n");
        let log = dir.join("m3u-viewer.log");
        let (_file, rotation_failure) = open_log_file(&log, date("2026-09-12")).unwrap();
        assert!(rotation_failure.is_none());
        assert!(dir.join("m3u-viewer.log.old").is_file());
        assert!(log.is_file(), "a fresh log was created");
        std::fs::remove_dir_all(dir).unwrap();
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
        let config = Config::default().with_xtream(Some(XtreamConfig::new(
            "http://example.com".to_owned(),
            "u".to_owned(),
            "p".to_owned(),
        )));
        let args = parse_args(std::iter::empty(), &config).unwrap();
        assert!(matches!(args.source, Source::Xtream(_)));
        assert_eq!(args.display_name, "xtream:example.com");
    }

    #[test]
    fn config_vlc_path_fallback() {
        let config = Config::default().with_vlc_path(Some(PathBuf::from("/usr/bin/vlc")));
        let args = parse_args(["list.m3u"].iter().map(OsString::from), &config).unwrap();
        assert_eq!(args.vlc_override, Some(PathBuf::from("/usr/bin/vlc")));
    }

    #[test]
    fn cli_vlc_overrides_config() {
        let config = Config::default().with_vlc_path(Some(PathBuf::from("/usr/bin/vlc")));
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
        let config = Config::default().with_user_agent(Some("FromConfig/1.0".to_owned()));
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
        let config = Config::default().with_epg_url(Some("http://config/epg.xml".to_owned()));
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
        let config = Config::default().with_vlc_reuse_instance(true);
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
    fn file_source_save_preserves_xtream_credentials_and_regex_setting() {
        let current = Config::default()
            .with_xtream(Some(XtreamConfig::new(
                "http://example.com".to_owned(),
                "stored-user".to_owned(),
                "stored-password".to_owned(),
            )))
            .with_regex_filter(false);
        let args = parse_args(
            ["list.m3u", "--save-config"].iter().map(OsString::from),
            &current,
        )
        .unwrap();

        let saved = config_to_save(&args, &current);
        let xtream = saved.xtream().unwrap();
        assert_eq!(xtream.server(), "http://example.com");
        assert_eq!(xtream.username(), "stored-user");
        assert_eq!(xtream.password(), "stored-password");
        assert!(!saved.regex_filter());
    }

    #[test]
    fn save_preserves_disabled_vlc_prewarm() {
        let current = Config::default().with_vlc_prewarm(false);
        let args = parse_args(
            ["list.m3u", "--save-config"].iter().map(OsString::from),
            &current,
        )
        .unwrap();

        assert!(!config_to_save(&args, &current).vlc_prewarm());
    }

    /// Unique temp dir holding a `config.toml` with `contents`.
    fn config_with(tag: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "m3u-viewer-main-config-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    const SAVED_ACCOUNT: &str = "[xtream]\nserver = \"http://example.com\"\n\
        username = \"stored-user\"\npassword = \"stored-password\"\n";

    #[test]
    fn save_config_refuses_to_overwrite_an_unreadable_config() {
        // Regression: a config that failed to parse fell back to defaults,
        // and --save-config then wrote those defaults over it — deleting
        // the stored Xtream credentials along with the typo.
        let corrupt = format!("{SAVED_ACCOUNT}regex_filter = maybe\n");
        let path = config_with("corrupt", &corrupt);
        let startup = load_config(Some(&path));
        assert!(startup.unreadable.is_some());
        assert!(startup.config.xtream().is_none(), "falls back to defaults");

        let args = parse_args(
            ["list.m3u", "--save-config"].iter().map(OsString::from),
            &startup.config,
        )
        .unwrap();
        let error = save_config(&args, &startup, Some(&path)).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("delete"), "{message}");
        assert!(message.contains("regex_filter"), "names the parse error");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn save_config_keeps_the_account_of_a_readable_config() {
        let path = config_with("readable", SAVED_ACCOUNT);
        let startup = load_config(Some(&path));
        assert!(startup.unreadable.is_none());
        let args = parse_args(
            ["list.m3u", "--save-config", "--vlc", "/opt/vlc"]
                .iter()
                .map(OsString::from),
            &startup.config,
        )
        .unwrap();
        save_config(&args, &startup, Some(&path)).unwrap();
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.xtream().unwrap().username(), "stored-user");
        assert_eq!(saved.vlc_path(), Some(Path::new("/opt/vlc")));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn unreadable_config_yields_a_one_line_status_notice() {
        // The stderr warning is hidden behind the alternate screen, so the
        // status bar must say that defaults are in use.
        let path = config_with("notice", "not valid toml [[[");
        let startup = load_config(Some(&path));
        let notice = startup.unreadable.unwrap().notice();
        assert!(notice.contains(&path.display().to_string()));
        assert!(notice.contains("using defaults"));
        assert!(!notice.contains('\n'));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_config_is_not_reported_as_unreadable() {
        let path = std::env::temp_dir().join("m3u-viewer-main-config-absent-4242.toml");
        let startup = load_config(Some(&path));
        assert!(startup.unreadable.is_none());
        assert!(load_config(None).unreadable.is_none());
    }

    #[test]
    fn failed_explicit_epg_keeps_the_playlist_source_for_retry() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut runtime = EpgRuntime {
            rx: Some(rx),
            user_agent: None,
            active_playlist_url: None,
            pending_playlist_url: None,
            resolved: false,
        };

        assert!(!runtime.observe_playlist_url("fallback.xml"));
        assert_eq!(
            runtime.pending_playlist_url.as_deref(),
            Some("fallback.xml")
        );
        tx.send(EpgEvent::Failed("primary failed".to_owned()))
            .unwrap();
        assert!(matches!(runtime.take_event(), Some(EpgEvent::Failed(_))));
        assert!(runtime.rx.is_none());
        assert!(!runtime.resolved);
        assert!(runtime.start_pending());
        assert_eq!(runtime.active_playlist_url.as_deref(), Some("fallback.xml"));
    }

    #[test]
    fn successful_epg_discards_and_blocks_playlist_sources() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut runtime = EpgRuntime {
            rx: Some(rx),
            user_agent: None,
            active_playlist_url: None,
            pending_playlist_url: Some("fallback.xml".to_owned()),
            resolved: false,
        };

        tx.send(EpgEvent::Loaded(Guide::default())).unwrap();
        assert!(matches!(runtime.take_event(), Some(EpgEvent::Loaded(_))));
        assert!(runtime.rx.is_none());
        assert!(runtime.resolved);
        assert!(runtime.pending_playlist_url.is_none());
        assert!(!runtime.observe_playlist_url("second.xml"));
    }

    #[test]
    fn duplicate_inflight_playlist_epg_url_is_ignored() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut runtime = EpgRuntime {
            rx: Some(rx),
            user_agent: None,
            active_playlist_url: Some("guide.xml".to_owned()),
            pending_playlist_url: None,
            resolved: false,
        };

        assert!(!runtime.observe_playlist_url("guide.xml"));
        assert!(runtime.pending_playlist_url.is_none());
    }

    #[test]
    fn cli_credentials_override_config() {
        // Regression: partial CLI credentials must win over stored ones
        // rather than being silently replaced by the whole config block.
        let config = Config::default().with_xtream(Some(XtreamConfig::new(
            "http://example.com".to_owned(),
            "stored".to_owned(),
            "stored-pw".to_owned(),
        )));
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
