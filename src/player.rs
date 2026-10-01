//! External playback: locating a VLC executable and launching streams in
//! it as a detached process, so the viewer keeps running.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// Why a stream could not be handed to VLC.
#[derive(Debug, Error)]
pub enum PlayerError {
    /// No usable VLC executable was found during discovery.
    #[error("VLC not found — checked --vlc, PATH, and standard install folders")]
    NotFound,
    /// A `--vlc` override was given but does not point at an executable file.
    #[error("--vlc path is not an executable file: {0}")]
    BadOverride(PathBuf),
    /// VLC was found but could not be started.
    #[error("failed to launch VLC: {0}")]
    Spawn(#[from] std::io::Error),
    /// The entry's URL failed the pre-launch safety check, so VLC was not
    /// started at all.
    #[error("refusing to play: {0}")]
    UnsafeUrl(#[from] UrlRejection),
}

/// Why a playlist entry's URL was not handed to VLC.
///
/// Playlists are untrusted input and the parser accepts any non-`#` line
/// as a URL, so a hostile entry could otherwise smuggle VLC options in or
/// make VLC open a network file share (which on Windows leaks the user's
/// NTLM hash over SMB). The messages deliberately never echo the URL, as
/// Xtream URLs carry the account credentials.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UrlRejection {
    /// Starts with `-`, so VLC would parse it as a command-line option.
    #[error("stream URL starts with '-' and would be read as a VLC option")]
    LeadingDash,
    /// A UNC / network share path (`\\host\share`, `//host/share`).
    #[error("stream URL is a network file share path")]
    NetworkPath,
    /// Not of the form `scheme://…`, e.g. a local file path.
    #[error("stream URL is not a network address of the form scheme://…")]
    NotNetworkUrl,
    /// A well-formed URL whose scheme is not a known streaming protocol,
    /// e.g. `file:` or `smb:`.
    #[error("stream URL scheme is not supported (allowed: {})", ALLOWED_SCHEMES.join(", "))]
    UnsupportedScheme,
}

/// Network streaming protocols VLC may be pointed at, compared
/// case-insensitively. Everything else — notably `file:`, `smb:`, and
/// VLC-internal schemes such as `vlc:` or `screen:` — is refused.
pub const ALLOWED_SCHEMES: &[&str] = &[
    "http", "https", "rtsp", "rtsps", "rtmp", "rtmps", "rtp", "udp", "mms", "mmsh", "srt",
];

/// Checks that `url` is a network stream VLC can safely be launched on.
///
/// Local file paths are refused too: playlists in this viewer are IPTV
/// channel lists, and playing local files from them is not a feature.
///
/// # Errors
///
/// The [`UrlRejection`] describing the first problem found.
pub fn check_stream_url(url: &str) -> Result<(), UrlRejection> {
    if url.starts_with('-') {
        return Err(UrlRejection::LeadingDash);
    }
    let mut leading = url.chars();
    if matches!(
        (leading.next(), leading.next()),
        (Some('/' | '\\'), Some('/' | '\\'))
    ) {
        return Err(UrlRejection::NetworkPath);
    }
    // RFC 3986: scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ).
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err(UrlRejection::NotNetworkUrl);
    };
    let well_formed = scheme
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    // A one-letter "scheme" is a Windows drive letter (`C:\…`).
    if !well_formed || scheme.len() < 2 {
        return Err(UrlRejection::NotNetworkUrl);
    }
    if !ALLOWED_SCHEMES
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
    {
        return Err(UrlRejection::UnsupportedScheme);
    }
    if !rest.starts_with("//") {
        return Err(UrlRejection::NotNetworkUrl);
    }
    Ok(())
}

/// A resolved external player.
#[derive(Debug)]
pub struct Player {
    exe: PathBuf,
    /// When set, playback requests are handed to VLC's single-instance IPC
    /// (`--one-instance`) instead of spawning a separate player window per
    /// channel.
    reuse_instance: bool,
}

impl Player {
    /// Locates VLC: an explicit `--vlc` override wins, then `vlc` on
    /// `PATH`, then the platform's standard install locations.
    ///
    /// # Errors
    ///
    /// [`PlayerError::BadOverride`] if `override_path` is set but is not
    /// executable; [`PlayerError::NotFound`] if discovery comes up empty.
    pub fn discover(override_path: Option<&Path>) -> Result<Self, PlayerError> {
        if let Some(path) = override_path {
            return if is_executable_file(path) {
                log::info!("using VLC override: {}", path.display());
                Ok(Self {
                    exe: path.to_path_buf(),
                    reuse_instance: false,
                })
            } else {
                log::warn!("VLC override is not executable: {}", path.display());
                Err(PlayerError::BadOverride(path.to_path_buf()))
            };
        }
        let path_dirs = env::var_os("PATH")
            .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default();
        let result = find_executable(path_dirs.iter().map(PathBuf::as_path))
            .or_else(|| find_executable(standard_dirs().iter().map(PathBuf::as_path)))
            .map(|exe| Self {
                exe,
                reuse_instance: false,
            })
            .ok_or(PlayerError::NotFound);
        match &result {
            Ok(player) => log::info!("VLC found: {}", player.exe.display()),
            Err(_) => log::warn!("VLC not found"),
        }
        result
    }

    /// Sets whether playback requests reuse a single running VLC instance
    /// (via `--one-instance`) rather than opening a new window each time.
    #[must_use]
    pub fn with_reuse_instance(mut self, enabled: bool) -> Self {
        self.reuse_instance = enabled;
        self
    }

    /// Full path of the executable that will be launched.
    #[must_use]
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Launches `url` in VLC, detached: output is discarded and a reaper
    /// thread waits on the child so the viewer neither blocks nor leaves
    /// zombies behind.
    ///
    /// When [`Self::with_reuse_instance`] is enabled, `--one-instance` and
    /// `--no-playlist-enqueue` are passed so VLC hands the URL off to an
    /// already-running instance and plays it immediately (replacing
    /// whatever it was playing) instead of spawning a new window; the
    /// first launch still starts VLC normally since no instance exists yet.
    /// On Linux this relies on VLC's D-Bus single-instance support, which
    /// may be unavailable in headless or minimal desktop environments.
    ///
    /// Timings are written to the log: how long the spawn call itself
    /// blocked the UI thread, how long VLC then took to become ready for
    /// input, and how long the child lived. In `--one-instance` mode the
    /// child exits as soon as the URL has been handed to the running VLC,
    /// so its lifetime measures the handoff.
    ///
    /// # Errors
    ///
    /// [`PlayerError::UnsafeUrl`] if `url` fails [`check_stream_url`], in
    /// which case VLC is not started; [`PlayerError::Spawn`] if the process
    /// cannot be started.
    pub fn play(&self, url: &str) -> Result<(), PlayerError> {
        log::info!("launching VLC for playback");
        let started = Instant::now();
        let (mut child, spawn_time) = self.spawn_detached(url)?;
        let pid = child.id();
        log::info!(
            "VLC spawn took {} ms (pid {pid}, reuse_instance={})",
            spawn_time.as_millis(),
            self.reuse_instance
        );
        thread::spawn(move || {
            report_ready(&child, pid, started);
            let status = child.wait();
            log::info!(
                "VLC process {pid} ended after {} ms: {}",
                started.elapsed().as_millis(),
                match status {
                    Ok(status) => status.to_string(),
                    Err(error) => format!("wait failed: {error}"),
                }
            );
        });
        Ok(())
    }

    /// Reads VLC's executable, libraries, and plugins in a background
    /// thread so the first playback does not pay for them.
    ///
    /// Measured on Windows, a first launch after a reboot took ~10.7 s to
    /// become ready against ~0.3 s warm. Almost none of that is VLC's own
    /// initialisation: it is paging ~134 MB of plugin DLLs off disk and,
    /// on Windows, the on-access virus scan of each one. Reading them here
    /// moves both costs off the critical path and onto a thread nobody is
    /// waiting for.
    ///
    /// Returns immediately; progress and totals go to the log. Errors are
    /// deliberately swallowed — a warm cache is an optimisation, and
    /// failing to read a file must never affect playback.
    pub fn prewarm(&self) {
        let exe = self.exe.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let targets = prewarm_targets(&exe);
            let mut warmed = 0u64;
            let mut files = 0usize;
            for target in &targets {
                if warmed >= PREWARM_BYTE_BUDGET {
                    log::warn!("VLC pre-warm stopped at the {PREWARM_BYTE_BUDGET} byte budget");
                    break;
                }
                if let Ok(bytes) = warm_file(target) {
                    warmed += bytes;
                    files += 1;
                }
            }
            log::info!(
                "VLC pre-warmed: {files} files, {} MB in {} ms",
                warmed / (1024 * 1024),
                started.elapsed().as_millis()
            );
        });
    }

    /// Spawns VLC on `url` with its streams detached, reporting how long
    /// the spawn call took. Refuses URLs that fail [`check_stream_url`].
    fn spawn_detached(&self, url: &str) -> Result<(Child, Duration), PlayerError> {
        // Validated here, directly before the argument is built, so no
        // caller can reach the spawn with an unchecked URL.
        if let Err(rejection) = check_stream_url(url) {
            // The reason only: the URL may carry Xtream credentials.
            log::warn!("refused to launch VLC: {rejection}");
            return Err(rejection.into());
        }
        let mut command = Command::new(&self.exe);
        if self.reuse_instance {
            command.arg("--one-instance").arg("--no-playlist-enqueue");
        }
        // Defence in depth only — validation above is the real guard.
        // VLC's getopt (src/config/getopt.c) treats `--` as the end of
        // options, so even a URL that slipped past the check could not be
        // parsed as an option.
        command
            .arg("--")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = Instant::now();
        let child = command.spawn()?;
        Ok((child, started.elapsed()))
    }
}

/// Upper bound on how much VLC gets read during pre-warming, so an
/// unexpected install layout cannot turn startup into a disk hog.
const PREWARM_BYTE_BUDGET: u64 = 512 * 1024 * 1024;

/// Reads `path` and throws the bytes away, returning how many were read.
///
/// The point is the side effect: the file lands in the OS file cache, and
/// on Windows the on-access virus scan happens here rather than when VLC
/// loads it.
fn warm_file(path: &Path) -> io::Result<u64> {
    let mut file = fs::File::open(path)?;
    io::copy(&mut file, &mut io::sink())
}

/// Files worth warming: the executable itself, the DLLs beside it, and
/// everything in `plugins/`, which is the bulk of a VLC install.
fn prewarm_targets(exe: &Path) -> Vec<PathBuf> {
    let Some(install_dir) = exe.parent() else {
        return vec![exe.to_path_buf()];
    };
    let mut targets = vec![exe.to_path_buf()];
    let mut dirs = vec![install_dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(path),
                Ok(kind) if kind.is_file() => {
                    let worth_warming = path.extension().is_some_and(|extension| {
                        ["dll", "dat", "so", "dylib"]
                            .iter()
                            .any(|wanted| extension.eq_ignore_ascii_case(wanted))
                    });
                    if worth_warming {
                        targets.push(path);
                    }
                }
                _ => {}
            }
        }
    }
    targets
}

/// Logs how long VLC took to finish starting up and become ready for
/// input, measured from just before the spawn call.
///
/// This is the number that `spawn` itself cannot see: spawning returns in
/// a few milliseconds, while VLC goes on to load its plugin DLLs. On a
/// warm start those are already in the OS file cache and the wait is
/// short; on a cold start — the first launch after a reboot — they come
/// off disk and this is where the seconds go.
///
/// Blocking, so call it from the reaper thread, never the UI thread.
#[cfg(windows)]
fn report_ready(child: &Child, pid: u32, started: Instant) {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::Threading::WaitForInputIdle;

    /// VLC finished initialising and is waiting for input.
    const READY: u32 = 0;
    /// VLC is still busy after `READY_TIMEOUT_MS`.
    const TIMED_OUT: u32 = 0x0000_0102;
    /// Generous: a cold start on a slow disk can take a long time, and
    /// this only occupies the reaper thread.
    const READY_TIMEOUT_MS: u32 = 120_000;

    let handle = child.as_raw_handle();
    // SAFETY: `handle` belongs to `child`, which is borrowed for the whole
    // call, so the process handle stays open and valid throughout.
    // WaitForInputIdle only waits on it.
    #[allow(unsafe_code)] // No safe equivalent: std cannot wait on process readiness.
    let outcome = unsafe { WaitForInputIdle(handle, READY_TIMEOUT_MS) };
    let elapsed = started.elapsed().as_millis();
    match outcome {
        READY => log::info!("VLC ready after {elapsed} ms (pid {pid})"),
        TIMED_OUT => log::warn!("VLC still not ready after {elapsed} ms (pid {pid})"),
        // Defensive: a process that exits before it ever goes idle has no
        // readiness to report. Measured VLC does reach idle even in
        // --one-instance handoff mode, where "ready" means the URL has
        // been passed on, so this branch is not the normal handoff path.
        _ => log::debug!("VLC readiness not observable (pid {pid})"),
    }
}

/// Non-Windows stand-in: readiness cannot be observed portably, so the
/// spawn and lifetime timings are all the log gets.
#[cfg(not(windows))]
fn report_ready(_child: &Child, _pid: u32, _started: Instant) {}

/// First directory in `dirs` containing a VLC executable name.
fn find_executable<'a>(dirs: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["vlc.exe"]
    } else {
        &["vlc"]
    };
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

/// Standard VLC install directories for the current platform.
fn standard_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        ["ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(env::var_os)
            .map(|root| PathBuf::from(root).join("VideoLAN").join("VLC"))
            .collect()
    } else if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Applications/VLC.app/Contents/MacOS")]
    } else {
        vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/snap/bin"),
        ]
    }
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use super::*;

    /// Creates a unique temp dir containing a fake VLC executable.
    fn fake_vlc_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("m3u-viewer-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "vlc.exe" } else { "vlc" };
        let path = dir.join(name);
        fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[test]
    fn override_must_point_at_a_file() {
        let missing = Path::new("Z:/definitely/not/here/vlc.exe");
        assert!(matches!(
            Player::discover(Some(missing)),
            Err(PlayerError::BadOverride(_))
        ));
    }

    #[test]
    fn override_wins_when_valid() {
        let dir = fake_vlc_dir("override");
        let exe = dir.join(if cfg!(windows) { "vlc.exe" } else { "vlc" });
        let player = Player::discover(Some(&exe)).unwrap();
        assert_eq!(player.exe(), exe);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn override_rejects_a_non_executable_file() {
        let dir = env::temp_dir().join(format!(
            "m3u-viewer-test-bad-override-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(if cfg!(windows) { "vlc.txt" } else { "vlc" });
        fs::write(&path, b"not executable").unwrap();

        assert!(matches!(
            Player::discover(Some(&path)),
            Err(PlayerError::BadOverride(_))
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reuse_instance_is_disabled_by_default_and_toggled_explicitly() {
        let dir = fake_vlc_dir("reuse");
        let exe = dir.join(if cfg!(windows) { "vlc.exe" } else { "vlc" });
        let player = Player::discover(Some(&exe)).unwrap();
        assert!(!player.reuse_instance);
        let player = player.with_reuse_instance(true);
        assert!(player.reuse_instance);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn spawn_reports_how_long_the_launch_took() {
        // A harmless stand-in for VLC: exits immediately, whatever the
        // "url" argument is.
        let exe = if cfg!(windows) {
            PathBuf::from(env::var_os("SystemRoot").unwrap_or_else(|| "C:/Windows".into()))
                .join("System32")
                .join("where.exe")
        } else {
            PathBuf::from("/bin/echo")
        };
        if !is_executable_file(&exe) {
            return;
        }
        let player = Player::discover(Some(&exe)).unwrap();
        let (mut child, spawn_time) = player
            .spawn_detached("http://m3u-viewer-test.invalid/stream")
            .unwrap();
        let pid = child.id();
        // A console stand-in has no idle state to wait for, so this
        // exercises the unobservable branch: it must return, not hang or
        // panic, or every playback would leak a stuck reaper thread.
        report_ready(&child, pid, Instant::now());
        child.wait().unwrap();
        assert!(spawn_time < Duration::from_secs(30));
    }

    #[test]
    fn network_stream_urls_are_accepted() {
        for url in [
            "http://example.com/live/1.ts",
            "https://example.com/user/pass/42.m3u8",
            "rtsp://cam.local:554/stream",
            "rtsps://cam.local/stream",
            "rtmp://example.com/app/key",
            "rtmps://example.com/app/key",
            "rtp://@239.0.0.1:5004",
            "udp://@239.0.0.1:1234",
            "mms://example.com/stream",
            "mmsh://example.com/stream",
            "srt://example.com:9000",
        ] {
            assert_eq!(check_stream_url(url), Ok(()), "{url}");
        }
    }

    #[test]
    fn scheme_match_ignores_case() {
        assert_eq!(check_stream_url("HTTP://example.com/a.ts"), Ok(()));
        assert_eq!(check_stream_url("HtTpS://example.com/a.ts"), Ok(()));
        assert_eq!(
            check_stream_url("FiLe://evil/share/x.ts"),
            Err(UrlRejection::UnsupportedScheme)
        );
        assert_eq!(
            check_stream_url("SMB://evil/share/x.ts"),
            Err(UrlRejection::UnsupportedScheme)
        );
    }

    #[test]
    fn leading_dash_is_rejected_as_an_option() {
        for url in [
            r"--config=\\evil\share\vlcrc",
            "-I",
            "--extraintf=http",
            "-http://example.com/a.ts",
        ] {
            assert_eq!(
                check_stream_url(url),
                Err(UrlRejection::LeadingDash),
                "{url}"
            );
        }
    }

    #[test]
    fn unc_paths_are_rejected() {
        for url in [
            r"\\evil\share\a.ts",
            "//evil/share/a.ts",
            r"\/evil/share/a.ts",
            r"/\evil\share\a.ts",
            r"\\?\UNC\evil\share\a.ts",
        ] {
            assert_eq!(
                check_stream_url(url),
                Err(UrlRejection::NetworkPath),
                "{url}"
            );
        }
    }

    #[test]
    fn file_and_smb_schemes_are_rejected() {
        for url in [
            "file://evil/share/x.ts",
            "file:///C:/Users/me/video.ts",
            "file:x.ts",
            "smb://evil/share/x.ts",
            "smb:x",
        ] {
            assert_eq!(
                check_stream_url(url),
                Err(UrlRejection::UnsupportedScheme),
                "{url}"
            );
        }
    }

    #[test]
    fn unknown_schemes_are_rejected() {
        for url in [
            "ftp://example.com/a.ts",
            "vlc://quit",
            "screen://",
            "dshow://",
            "javascript:alert(1)",
            "http/ts://example.com/a.ts",
        ] {
            assert!(check_stream_url(url).is_err(), "{url}");
        }
        assert_eq!(
            check_stream_url("ftp://example.com/a.ts"),
            Err(UrlRejection::UnsupportedScheme)
        );
    }

    #[test]
    fn local_paths_and_malformed_urls_are_rejected() {
        for url in [
            "",
            "channel.ts",
            "/home/me/video.ts",
            r"C:\Videos\a.ts",
            "C:/Videos/a.ts",
            " http://example.com/a.ts",
            ":sout=#file{dst=x}",
            "http:example.com/a.ts",
            "http:\\\\evil\\share",
        ] {
            assert_eq!(
                check_stream_url(url),
                Err(UrlRejection::NotNetworkUrl),
                "{url}"
            );
        }
    }

    #[test]
    fn rejection_messages_never_echo_the_url() {
        let url = "smb://user:secret@evil/share/x.ts";
        let message = PlayerError::from(check_stream_url(url).unwrap_err()).to_string();
        assert!(message.starts_with("refusing to play"), "{message}");
        assert!(!message.contains("secret"), "{message}");
        assert!(!message.contains("evil"), "{message}");
    }

    #[test]
    fn hostile_urls_never_reach_the_spawn() {
        // Regression: a playlist line like `--config=…` or a UNC path used
        // to be passed to VLC verbatim. The stand-in executable must not
        // even be started.
        let dir = fake_vlc_dir("hostile");
        let exe = dir.join(if cfg!(windows) { "vlc.exe" } else { "vlc" });
        let player = Player::discover(Some(&exe)).unwrap();
        for url in [r"--config=\\evil\share\vlcrc", r"\\evil\share\a.ts"] {
            assert!(matches!(player.play(url), Err(PlayerError::UnsafeUrl(_))));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prewarm_collects_the_exe_and_its_libraries_recursively() {
        let dir = fake_vlc_dir("prewarm");
        let exe = dir.join(if cfg!(windows) { "vlc.exe" } else { "vlc" });
        fs::write(dir.join("libvlccore.dll"), b"lib").unwrap();
        fs::create_dir_all(dir.join("plugins/codec")).unwrap();
        fs::write(dir.join("plugins/plugins.dat"), b"cache").unwrap();
        fs::write(dir.join("plugins/codec/libavcodec_plugin.dll"), b"plugin").unwrap();
        // Not a library: must not be read just because it sits alongside.
        fs::write(dir.join("NEWS.txt"), b"release notes").unwrap();

        let targets = prewarm_targets(&exe);
        let names: Vec<String> = targets
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&exe.file_name().unwrap().to_string_lossy().into_owned()));
        assert!(names.contains(&"libvlccore.dll".to_owned()));
        assert!(names.contains(&"plugins.dat".to_owned()));
        assert!(
            names.contains(&"libavcodec_plugin.dll".to_owned()),
            "nested plugin dirs must be walked"
        );
        assert!(!names.contains(&"NEWS.txt".to_owned()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn warming_reads_the_whole_file_and_missing_files_are_not_fatal() {
        let dir = fake_vlc_dir("warmfile");
        let path = dir.join("payload.dll");
        fs::write(&path, vec![7u8; 4096]).unwrap();
        assert_eq!(warm_file(&path).unwrap(), 4096);
        // A file that vanished between listing and reading must not stop
        // the sweep; prewarm ignores the error and moves on.
        assert!(warm_file(&dir.join("gone.dll")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finds_executable_in_candidate_dirs() {
        let dir = fake_vlc_dir("dirs");
        let missing = env::temp_dir().join("m3u-viewer-test-empty-nonexistent");
        let found = find_executable([missing.as_path(), dir.as_path()].into_iter());
        assert_eq!(found.unwrap().parent().unwrap(), dir);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_everywhere_is_none() {
        let missing = env::temp_dir().join("m3u-viewer-test-empty-nonexistent");
        assert!(find_executable(std::iter::once(missing.as_path())).is_none());
    }
}
