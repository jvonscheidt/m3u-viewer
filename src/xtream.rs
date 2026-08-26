//! Xtream Codes account support.
//!
//! Instead of a local file, the playlist can come from an Xtream Codes
//! server: `get.php?type=m3u_plus` returns the account's channels as a
//! regular extended M3U, which streams through the normal parser. Some
//! panels disable that M3U download; for those, [`Account`] also exposes
//! the JSON player API (`player_api.php`) — [`Category`] and
//! [`LiveStream`] lists from which the loader synthesizes the channel
//! list itself.
//!
//! Xtream embeds credentials in request URLs. Those URLs must never be
//! logged; diagnostics report only status, content metadata, and redirect
//! destination origins.

use std::fmt;
use std::io::Read;
use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use thiserror::Error;
use ureq::ResponseExt as _;

#[cfg(not(test))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const PLAYLIST_TIMEOUT: Duration = Duration::from_mins(1);
#[cfg(test)]
const PLAYLIST_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const API_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const API_TIMEOUT: Duration = Duration::from_millis(500);

/// Why the playlist could not be fetched from the server.
#[derive(Debug, Error)]
pub enum XtreamError {
    /// The server replied, but not with the playlist.
    #[error("server returned HTTP {0} — check server URL and credentials")]
    Status(u16),
    /// The request itself failed (DNS, connect, TLS, …).
    #[error("request failed: {0}")]
    Http(#[from] Box<ureq::Error>),
    /// The player API's JSON reply could not be parsed.
    #[error("could not parse the server's reply: {0}")]
    Json(#[from] serde_json::Error),
    /// The panel reported that these credentials are not authorized.
    #[error("Xtream authentication failed — check username, password, and account status")]
    AuthFailed,
    /// The player API returned valid JSON, but not the requested list.
    #[error("player API returned an unexpected reply instead of a channel list")]
    UnexpectedApiReply,
}

/// One live category from `player_api.php?action=get_live_categories`.
#[derive(Debug, Deserialize)]
pub struct Category {
    /// Panel-assigned id, referenced by [`LiveStream::category_id`].
    #[serde(rename = "category_id", deserialize_with = "required_scalar")]
    pub(crate) id: String,
    /// Human-readable name; becomes the channel group.
    #[serde(rename = "category_name")]
    pub(crate) name: String,
}

impl Category {
    /// Panel-assigned category identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Human-readable category name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One live stream from `player_api.php?action=get_live_streams`.
#[derive(Debug, Deserialize)]
pub struct LiveStream {
    /// Display name; `None` when the panel sent none.
    #[serde(default, deserialize_with = "lenient_scalar")]
    pub(crate) name: Option<String>,
    /// Id from which [`Account::live_stream_url`] builds the URL.
    #[serde(deserialize_with = "lenient_u64")]
    pub(crate) stream_id: u64,
    /// Category (group) of the stream, when the panel sets one.
    #[serde(default, deserialize_with = "lenient_scalar")]
    pub(crate) category_id: Option<String>,
    /// EPG channel id (`tvg-id` equivalent), when set.
    #[serde(default, deserialize_with = "lenient_scalar")]
    pub(crate) epg_channel_id: Option<String>,
}

impl LiveStream {
    /// Display name supplied by the panel.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Panel-assigned stream identifier.
    #[must_use]
    pub fn stream_id(&self) -> u64 {
        self.stream_id
    }

    /// Category identifier supplied by the panel.
    #[must_use]
    pub fn category_id(&self) -> Option<&str> {
        self.category_id.as_deref()
    }

    /// XMLTV channel identifier supplied by the panel.
    #[must_use]
    pub fn epg_channel_id(&self) -> Option<&str> {
        self.epg_channel_id.as_deref()
    }
}

/// Panels are inconsistent about JSON scalar types — ids arrive as
/// numbers or strings, optional fields as `null` or `""`. Normalizes all
/// of that to an optional string.
fn scalar_to_string(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) if !text.is_empty() => Some(text),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn lenient_scalar<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(scalar_to_string(value))
}

fn required_scalar<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    scalar_to_string(value).ok_or_else(|| serde::de::Error::custom("expected a string or number"))
}

fn lenient_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match &value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    }
    .ok_or_else(|| serde::de::Error::custom("expected an unsigned number"))
}

/// Replaces anything that isn't ASCII alphanumeric with `_`, so the result
/// is safe to use as a path component on every platform.
fn sanitize_for_filename(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Stable non-cryptographic identity suffix preventing sanitized-name collisions.
fn cache_identity_hash(host: &str, username: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    host.bytes()
        .chain(std::iter::once(0))
        .chain(username.bytes())
        .fold(OFFSET_BASIS, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(RESPONSE_TIMEOUT))
        .max_redirects(3)
        .max_redirects_will_error(true)
        .save_redirect_history(true)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Credentials for one Xtream Codes account.
pub struct Account {
    server: String,
    username: String,
    password: String,
    /// Custom `User-Agent` header; `None` keeps the HTTP client's default.
    user_agent: Option<String>,
    agent: ureq::Agent,
}

impl fmt::Debug for Account {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Account")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl Account {
    /// Creates an account handle. `server` may omit the scheme
    /// (`http://` is assumed, as most providers are plain HTTP) and may
    /// carry a trailing slash.
    #[must_use]
    pub fn new(server: &str, username: String, password: String) -> Self {
        let server = server.trim_end_matches('/');
        let server = if server.contains("://") {
            server.to_owned()
        } else {
            format!("http://{server}")
        };
        Self {
            server,
            username,
            password,
            user_agent: None,
            agent: http_agent(),
        }
    }

    /// Sends `user_agent` as the `User-Agent` header on playlist requests;
    /// some providers only answer to known player user agents. `None`
    /// keeps the HTTP client's default.
    #[must_use]
    pub fn with_user_agent(mut self, user_agent: Option<String>) -> Self {
        self.user_agent = user_agent;
        self
    }

    /// Returns `(server, username, password)` for persisting to a config file.
    #[must_use]
    pub fn credentials(&self) -> (&str, &str, &str) {
        (&self.server, &self.username, &self.password)
    }

    /// Filesystem-safe key identifying this account for the on-disk
    /// playlist cache: the server host and username (not the password),
    /// with anything that isn't ASCII alphanumeric replaced by `_`, plus
    /// a stable hash of the unsanitized values to prevent collisions.
    #[must_use]
    pub fn cache_key(&self) -> String {
        let host = self
            .server
            .split_once("://")
            .map_or(self.server.as_str(), |(_, rest)| rest);
        let readable = format!(
            "{}-{}",
            sanitize_for_filename(host),
            sanitize_for_filename(&self.username)
        );
        let hash = cache_identity_hash(host, &self.username);
        format!("{readable}-{hash:016x}")
    }

    /// Host portion of the server URL, for display in the status bar.
    #[must_use]
    pub fn display_name(&self) -> String {
        let host = self
            .server
            .split_once("://")
            .map_or(self.server.as_str(), |(_, rest)| rest);
        format!("xtream:{host}")
    }

    /// The `get.php` URL that returns this account's playlist as
    /// extended M3U (credentials percent-encoded).
    #[must_use]
    pub fn playlist_url(&self) -> String {
        let (username, password) = self.encoded_credentials();
        format!(
            "{}/get.php?username={}&password={}&type=m3u_plus&output=ts",
            self.server, username, password,
        )
    }

    /// Issues a GET for `url` (custom user agent applied) and returns the
    /// response only when it is a 2xx.
    fn request(
        &self,
        url: String,
        timeout: Duration,
    ) -> Result<ureq::http::Response<ureq::Body>, XtreamError> {
        let mut request = self.agent.get(url);
        if let Some(ref user_agent) = self.user_agent {
            request = request.header("User-Agent", user_agent);
        }
        let response = match request
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(code)) => return Err(XtreamError::Status(code)),
            Err(other) => return Err(XtreamError::Http(Box::new(other))),
        };
        // ureq only turns 4xx/5xx into errors; panels answer with custom
        // codes like 884, which must not pass for success either.
        if !response.status().is_success() {
            return Err(XtreamError::Status(response.status().as_u16()));
        }
        log_redirect(&response);
        Ok(response)
    }

    /// Requests the playlist, returning a streaming body reader and the
    /// total size, when the server announces one (many send chunked
    /// responses, so progress may be indeterminate).
    ///
    /// # Errors
    ///
    /// [`XtreamError::Status`] for a non-success HTTP response,
    /// [`XtreamError::Http`] when the request cannot be made at all.
    pub fn fetch(&self) -> Result<(impl Read + use<>, Option<u64>), XtreamError> {
        let response = self.request(self.playlist_url(), PLAYLIST_TIMEOUT)?;
        let total = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<unknown>");
        log::info!(
            "xtream server answered HTTP {}, content-type: {content_type}, content-length: {total:?}",
            response.status()
        );
        // Unlimited body: playlists routinely exceed ureq's 10 MB default.
        let reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        Ok((reader, total))
    }

    /// The `xmltv.php` URL serving this account's EPG as an XMLTV
    /// document (credentials percent-encoded).
    #[must_use]
    pub fn xmltv_url(&self) -> String {
        let (username, password) = self.encoded_credentials();
        format!(
            "{}/xmltv.php?username={}&password={}",
            self.server, username, password,
        )
    }

    /// The `player_api.php` URL for `action` (credentials percent-encoded).
    fn api_url(&self, action: &str) -> String {
        let (username, password) = self.encoded_credentials();
        format!(
            "{}/player_api.php?username={}&password={}&action={action}",
            self.server, username, password,
        )
    }

    /// Downloads and parses `action`'s JSON array from the player API.
    fn fetch_api_list<T: serde::de::DeserializeOwned>(
        &self,
        action: &str,
    ) -> Result<Vec<T>, XtreamError> {
        let response = self.request(self.api_url(action), API_TIMEOUT)?;
        // Unlimited body: full stream lists routinely exceed ureq's
        // 10 MB default (55k streams ≈ 20 MB of JSON).
        let reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        let value: serde_json::Value = serde_json::from_reader(std::io::BufReader::new(reader))?;
        match &value {
            serde_json::Value::Array(_) => Ok(serde_json::from_value(value)?),
            serde_json::Value::Object(object) if api_auth_failed(object) => {
                Err(XtreamError::AuthFailed)
            }
            _ => Err(XtreamError::UnexpectedApiReply),
        }
    }

    /// Fetches the live categories (channel groups) from the player API.
    ///
    /// # Errors
    ///
    /// [`XtreamError`] when the request fails, the server answers with a
    /// non-2xx status, or the reply is not the expected JSON array.
    pub fn fetch_live_categories(&self) -> Result<Vec<Category>, XtreamError> {
        self.fetch_api_list("get_live_categories")
    }

    /// Fetches all live streams from the player API.
    ///
    /// # Errors
    ///
    /// [`XtreamError`] when the request fails, the server answers with a
    /// non-2xx status, or the reply is not the expected JSON array.
    pub fn fetch_live_streams(&self) -> Result<Vec<LiveStream>, XtreamError> {
        self.fetch_api_list("get_live_streams")
    }

    /// Playable URL for a live stream id, in the layout every Xtream
    /// panel serves: `/live/<user>/<pass>/<stream_id>.ts`.
    #[must_use]
    pub fn live_stream_url(&self, stream_id: u64) -> String {
        let (username, password) = self.encoded_credentials();
        format!(
            "{}/live/{}/{}/{stream_id}.ts",
            self.server, username, password,
        )
    }

    fn encoded_credentials(&self) -> (String, String) {
        (
            utf8_percent_encode(&self.username, NON_ALPHANUMERIC).to_string(),
            utf8_percent_encode(&self.password, NON_ALPHANUMERIC).to_string(),
        )
    }
}

fn api_auth_failed(object: &serde_json::Map<String, serde_json::Value>) -> bool {
    let Some(auth) = object
        .get("user_info")
        .and_then(serde_json::Value::as_object)
        .and_then(|user_info| user_info.get("auth"))
    else {
        return false;
    };
    matches!(auth, serde_json::Value::Bool(false))
        || auth.as_u64() == Some(0)
        || auth.as_str() == Some("0")
}

fn log_redirect(response: &ureq::http::Response<ureq::Body>) {
    let Some(history) = response.get_redirect_history() else {
        return;
    };
    if history.is_empty() {
        return;
    }
    let final_uri = response.get_uri();
    let destination = match (final_uri.scheme_str(), final_uri.authority()) {
        (Some(scheme), Some(authority)) => format!("{scheme}://{authority}"),
        (_, Some(authority)) => authority.to_string(),
        _ => "<relative URI>".to_owned(),
    };
    log::warn!(
        "xtream request followed {} redirect(s) to {destination}",
        history.len()
    );
}

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn scheme_is_added_and_slash_trimmed() {
        let account = Account::new("example.com:8080/", "u".into(), "p".into());
        assert_eq!(
            account.playlist_url(),
            "http://example.com:8080/get.php?username=u&password=p&type=m3u_plus&output=ts"
        );
    }

    #[test]
    fn https_scheme_is_kept() {
        let account = Account::new("https://example.com", "u".into(), "p".into());
        assert!(
            account
                .playlist_url()
                .starts_with("https://example.com/get.php")
        );
    }

    #[test]
    fn credentials_are_percent_encoded() {
        let account = Account::new("example.com", "user name".into(), "p&ss=1".into());
        assert_eq!(
            account.playlist_url(),
            "http://example.com/get.php?username=user%20name&password=p%26ss%3D1&type=m3u_plus&output=ts"
        );
    }

    #[test]
    fn xmltv_url_percent_encodes_credentials() {
        let account = Account::new("example.com", "user name".into(), "p&ss".into());
        assert_eq!(
            account.xmltv_url(),
            "http://example.com/xmltv.php?username=user%20name&password=p%26ss"
        );
    }

    #[test]
    fn display_name_is_the_host() {
        let account = Account::new("https://example.com:8080", "u".into(), "p".into());
        assert_eq!(account.display_name(), "xtream:example.com:8080");
    }

    #[test]
    fn cache_key_sanitizes_host_and_username_for_a_filename() {
        let account = Account::new("https://example.com:8080", "user name".into(), "p".into());
        let key = account.cache_key();
        let hash = key.strip_prefix("example_com_8080-user_name-").unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn cache_key_distinguishes_sanitization_collisions() {
        let punctuation = Account::new("a.b.com", "u-1".into(), "p".into());
        let underscores = Account::new("a_b_com", "u_1".into(), "p".into());
        assert_ne!(punctuation.cache_key(), underscores.cache_key());
    }

    #[test]
    fn cache_key_ignores_the_password() {
        let a = Account::new("example.com", "u".into(), "one".into());
        let b = Account::new("example.com", "u".into(), "two".into());
        assert_eq!(a.cache_key(), b.cache_key());
    }

    /// One-shot local HTTP server; returns the request it received.
    fn serve_once(
        status_line: &'static str,
        body: &'static str,
    ) -> (u16, std::thread::JoinHandle<String>) {
        use std::io::Write;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                if n == 0 || request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "{status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (port, handle)
    }

    #[test]
    fn fetches_playlist_over_http() {
        let body = "#EXTM3U\n#EXTINF:-1 group-title=\"News\",One\nhttp://u/1\n";
        let (port, server) = serve_once("HTTP/1.1 200 OK", body);
        let account = Account::new(&format!("127.0.0.1:{port}"), "user".into(), "pw".into());
        let (mut reader, total) = account.fetch().unwrap();
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, body);
        assert_eq!(total, Some(u64::try_from(body.len()).unwrap()));
        let request = server.join().unwrap();
        assert!(request.starts_with(
            "GET /get.php?username=user&password=pw&type=m3u_plus&output=ts HTTP/1.1"
        ));
    }

    #[test]
    fn custom_status_codes_are_errors_not_success() {
        // Panels use made-up codes like 884 to refuse the M3U download;
        // ureq only rejects 4xx/5xx by itself.
        let (port, server) = serve_once("HTTP/1.1 884 Blocked", "");
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let error = account.fetch().err().unwrap();
        assert!(matches!(error, XtreamError::Status(884)));
        let _ = server.join();
    }

    #[test]
    fn live_categories_parse_with_lenient_ids() {
        let body = r#"[{"category_id":1,"category_name":"News"},{"category_id":"2","category_name":"Sports"}]"#;
        let (port, server) = serve_once("HTTP/1.1 200 OK", body);
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let categories = account.fetch_live_categories().unwrap();
        let pairs: Vec<(&str, &str)> = categories
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(pairs, [("1", "News"), ("2", "Sports")]);
        let request = server.join().unwrap();
        assert!(request.starts_with(
            "GET /player_api.php?username=u&password=p&action=get_live_categories HTTP/1.1"
        ));
    }

    #[test]
    fn player_api_auth_object_has_an_actionable_error() {
        let body = r#"{"user_info":{"auth":0},"server_info":{}}"#;
        let (port, server) = serve_once("HTTP/1.1 200 OK", body);
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "wrong".into());
        let error = account.fetch_live_categories().unwrap_err();
        assert!(matches!(error, XtreamError::AuthFailed));
        assert!(error.to_string().contains("password"));
        let _ = server.join();
    }

    #[test]
    fn player_api_auth_failure_accepts_common_panel_scalar_types() {
        for auth in [
            serde_json::Value::Bool(false),
            serde_json::Value::Number(0.into()),
            serde_json::Value::String("0".into()),
        ] {
            let object = serde_json::json!({"user_info": {"auth": auth}});
            assert!(api_auth_failed(object.as_object().unwrap()));
        }
    }

    #[test]
    fn unexpected_player_api_object_is_not_reported_as_invalid_json() {
        let body = r#"{"error":"maintenance"}"#;
        let (port, server) = serve_once("HTTP/1.1 200 OK", body);
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let error = account.fetch_live_categories().unwrap_err();
        assert!(matches!(error, XtreamError::UnexpectedApiReply));
        let _ = server.join();
    }

    #[test]
    fn live_streams_parse_with_lenient_fields() {
        // stream_id as string, category_id as number/null, epg id and
        // name missing or empty — all real-world panel output.
        let body = r#"[
            {"name":"One","stream_id":11,"category_id":"7","epg_channel_id":"one.tv"},
            {"name":"","stream_id":"22","category_id":8,"epg_channel_id":""},
            {"stream_id":33,"category_id":null}
        ]"#;
        let (port, server) = serve_once("HTTP/1.1 200 OK", body);
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let streams = account.fetch_live_streams().unwrap();
        let _ = server.join();
        assert_eq!(streams.len(), 3);
        assert_eq!(streams[0].name.as_deref(), Some("One"));
        assert_eq!(streams[0].stream_id, 11);
        assert_eq!(streams[0].category_id.as_deref(), Some("7"));
        assert_eq!(streams[0].epg_channel_id.as_deref(), Some("one.tv"));
        assert_eq!(streams[1].name, None);
        assert_eq!(streams[1].stream_id, 22);
        assert_eq!(streams[1].category_id.as_deref(), Some("8"));
        assert_eq!(streams[1].epg_channel_id, None);
        assert_eq!(streams[2].name, None);
        assert_eq!(streams[2].category_id, None);
    }

    #[test]
    fn live_stream_url_percent_encodes_credentials() {
        let account = Account::new("example.com", "user name".into(), "p&ss".into());
        assert_eq!(
            account.live_stream_url(42),
            "http://example.com/live/user%20name/p%26ss/42.ts"
        );
    }

    #[test]
    fn custom_user_agent_replaces_the_default() {
        let (port, server) = serve_once("HTTP/1.1 200 OK", "#EXTM3U\n");
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into())
            .with_user_agent(Some("VLC/3.0.20 LibVLC/3.0.20".into()));
        let (mut reader, _) = account.fetch().unwrap();
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        let request = server.join().unwrap();
        let user_agents: Vec<&str> = request
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .collect();
        // Exactly one User-Agent header, and it is ours — the client must
        // not append its own default next to the configured one.
        assert_eq!(user_agents.len(), 1, "request was: {request}");
        assert!(
            user_agents[0].ends_with("VLC/3.0.20 LibVLC/3.0.20"),
            "unexpected header: {}",
            user_agents[0]
        );
    }

    #[test]
    fn bad_credentials_surface_as_status_error() {
        let (port, server) = serve_once("HTTP/1.1 401 Unauthorized", "");
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "wrong".into());
        let error = account.fetch().err().unwrap();
        assert!(matches!(error, XtreamError::Status(401)));
        assert!(
            error
                .to_string()
                .contains("check server URL and credentials")
        );
        let _ = server.join();
    }

    #[test]
    fn stalled_server_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(PLAYLIST_TIMEOUT + Duration::from_millis(250));
        });
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let error = account.fetch().err().unwrap();
        assert!(matches!(error, XtreamError::Http(_)));
        let _ = server.join();
    }
}
