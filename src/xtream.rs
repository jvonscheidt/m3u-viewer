//! Xtream Codes account support.
//!
//! Instead of a local file, the playlist can come from an Xtream Codes
//! server: `get.php?type=m3u_plus` returns the account's channels as a
//! regular extended M3U, which streams through the normal parser. Only
//! the download differs; everything downstream is shared.

use std::io::Read;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
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
}

/// Credentials for one Xtream Codes account.
pub struct Account {
    server: String,
    username: String,
    password: String,
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
        }
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

    /// Requests the playlist, returning a streaming body reader and the
    /// total size, when the server announces one (many send chunked
    /// responses, so progress may be indeterminate).
    ///
    /// # Errors
    ///
    /// [`XtreamError::Status`] for a non-success HTTP response,
    /// [`XtreamError::Http`] when the request cannot be made at all.
    pub fn fetch(&self) -> Result<(impl Read + use<>, Option<u64>), XtreamError> {
        let response = match ureq::get(self.playlist_url()).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(code)) => return Err(XtreamError::Status(code)),
            Err(other) => return Err(XtreamError::Http(Box::new(other))),
        };
        let total = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        // Unlimited body: playlists routinely exceed ureq's 10 MB default.
        let reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        Ok((reader, total))
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
