//! Xtream Codes account support.
//!
//! Instead of a local file, the playlist can come from an Xtream Codes
//! server: `get.php?type=m3u_plus` returns the account's channels as a
//! regular extended M3U, which streams through the normal parser. Some
//! panels disable that M3U download; for those, [`Account`] also exposes
//! the JSON player API (`player_api.php`) — [`Category`] and
//! [`LiveStream`] lists from which the loader synthesizes the channel
//! list itself.

use std::io::Read;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use thiserror::Error;

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
}

/// One live category from `player_api.php?action=get_live_categories`.
#[derive(Deserialize)]
pub struct Category {
    /// Panel-assigned id, referenced by [`LiveStream::category_id`].
    #[serde(rename = "category_id", deserialize_with = "required_scalar")]
    pub id: String,
    /// Human-readable name; becomes the channel group.
    #[serde(rename = "category_name")]
    pub name: String,
}

/// One live stream from `player_api.php?action=get_live_streams`.
#[derive(Deserialize)]
pub struct LiveStream {
    /// Display name; `None` when the panel sent none.
    #[serde(default, deserialize_with = "lenient_scalar")]
    pub name: Option<String>,
    /// Id from which [`Account::live_stream_url`] builds the URL.
    #[serde(deserialize_with = "lenient_u64")]
    pub stream_id: u64,
    /// Category (group) of the stream, when the panel sets one.
    #[serde(default, deserialize_with = "lenient_scalar")]
    pub category_id: Option<String>,
    /// EPG channel id (`tvg-id` equivalent), when set.
    #[serde(default, deserialize_with = "lenient_scalar")]
    pub epg_channel_id: Option<String>,
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

/// Credentials for one Xtream Codes account.
pub struct Account {
    server: String,
    username: String,
    password: String,
    /// Custom `User-Agent` header; `None` keeps the HTTP client's default.
    user_agent: Option<String>,
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
    /// with anything that isn't ASCII alphanumeric replaced by `_`.
    #[must_use]
    pub fn cache_key(&self) -> String {
        let host = self
            .server
            .split_once("://")
            .map_or(self.server.as_str(), |(_, rest)| rest);
        format!(
            "{}-{}",
            sanitize_for_filename(host),
            sanitize_for_filename(&self.username)
        )
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
        format!(
            "{}/get.php?username={}&password={}&type=m3u_plus&output=ts",
            self.server,
            utf8_percent_encode(&self.username, NON_ALPHANUMERIC),
            utf8_percent_encode(&self.password, NON_ALPHANUMERIC),
        )
    }

    /// Issues a GET for `url` (custom user agent applied) and returns the
    /// response only when it is a 2xx.
    fn request(&self, url: String) -> Result<ureq::http::Response<ureq::Body>, XtreamError> {
        let mut request = ureq::get(url);
        if let Some(ref user_agent) = self.user_agent {
            request = request.header("User-Agent", user_agent);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(code)) => return Err(XtreamError::Status(code)),
            Err(other) => return Err(XtreamError::Http(Box::new(other))),
        };
        // ureq only turns 4xx/5xx into errors; panels answer with custom
        // codes like 884, which must not pass for success either.
        if !response.status().is_success() {
            return Err(XtreamError::Status(response.status().as_u16()));
        }
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
        let response = self.request(self.playlist_url())?;
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

    /// The `player_api.php` URL for `action` (credentials percent-encoded).
    fn api_url(&self, action: &str) -> String {
        format!(
            "{}/player_api.php?username={}&password={}&action={action}",
            self.server,
            utf8_percent_encode(&self.username, NON_ALPHANUMERIC),
            utf8_percent_encode(&self.password, NON_ALPHANUMERIC),
        )
    }

    /// Downloads and parses `action`'s JSON array from the player API.
    fn fetch_api_list<T: serde::de::DeserializeOwned>(
        &self,
        action: &str,
    ) -> Result<Vec<T>, XtreamError> {
        let response = self.request(self.api_url(action))?;
        // Unlimited body: full stream lists routinely exceed ureq's
        // 10 MB default (55k streams ≈ 20 MB of JSON).
        let reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        Ok(serde_json::from_reader(std::io::BufReader::new(reader))?)
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
        format!(
            "{}/live/{}/{}/{stream_id}.ts",
            self.server,
            utf8_percent_encode(&self.username, NON_ALPHANUMERIC),
            utf8_percent_encode(&self.password, NON_ALPHANUMERIC),
        )
    }
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
    fn display_name_is_the_host() {
        let account = Account::new("https://example.com:8080", "u".into(), "p".into());
        assert_eq!(account.display_name(), "xtream:example.com:8080");
    }

    #[test]
    fn cache_key_sanitizes_host_and_username_for_a_filename() {
        let account = Account::new("https://example.com:8080", "user name".into(), "p".into());
        assert_eq!(account.cache_key(), "example_com_8080-user_name");
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
}
