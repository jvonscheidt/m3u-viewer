//! HTTP GET with redirects followed here rather than inside ureq, so a
//! failure can say where the request ended up.
//!
//! A redirect from the provider to a different host that then fails —
//! typically on TLS, because the new host's certificate isn't the
//! provider's — is the signature of DNS filtering or spoofing diverting
//! the provider's domain. ureq only reports redirect history on success,
//! so errors would otherwise blame "certificate expired" with no hint
//! that the request never reached the provider. Only scheme and host are
//! ever logged or reported: paths and queries carry the credentials.

use ureq::http::Uri;
use ureq::http::header::LOCATION;

/// Redirects followed before giving up, as before (ureq's limit was 3).
const MAX_REDIRECTS: u32 = 3;

/// Why [`get`] failed.
#[derive(Debug)]
pub(crate) enum GetErrorKind {
    /// The server replied with a non-success status (including a redirect
    /// that can't be followed).
    Status(u16),
    /// The request itself failed (DNS, connect, TLS, too many redirects, …).
    Request(Box<ureq::Error>),
}

/// A failed [`get`], with the host a redirect had moved it to, if any.
#[derive(Debug)]
pub(crate) struct GetError {
    /// What went wrong on the last request.
    pub(crate) kind: GetErrorKind,
    /// `scheme://host[:port]` of the last request when a redirect had
    /// taken it away from the original host.
    pub(crate) diverted_to: Option<String>,
}

/// Explanation put in front of errors after a redirect to a different
/// host — first, so a long TLS error after it can't push it out of view.
pub(crate) const DIVERTED_HINT: &str =
    "likely DNS filtering or DNS spoofing on your network diverting the server's domain";

/// GETs `url` with `agent` (which must not follow redirects itself, see
/// [`crate::xtream::http_agent`]), following up to [`MAX_REDIRECTS`]
/// redirects, and returns the response only when it is a 2xx.
///
/// # Errors
///
/// [`GetError`] for a failed request or a non-success status; when a
/// redirect had left the original host, it names the new one.
pub(crate) fn get(
    agent: &ureq::Agent,
    url: &str,
    user_agent: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>, GetError> {
    let original_host = host(url);
    let mut current = url.to_owned();
    let mut diverted_to: Option<String> = None;
    let mut redirects = 0;
    loop {
        let fail = |kind| GetError {
            kind,
            diverted_to: diverted_to.clone(),
        };
        let mut request = agent.get(&current);
        if let Some(user_agent) = user_agent {
            request = request.header("User-Agent", user_agent);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(code)) => return Err(fail(GetErrorKind::Status(code))),
            Err(other) => return Err(fail(GetErrorKind::Request(Box::new(other)))),
        };
        let status = response.status();
        if status.is_redirection() {
            if redirects == MAX_REDIRECTS {
                return Err(fail(GetErrorKind::Request(Box::new(
                    ureq::Error::TooManyRedirects,
                ))));
            }
            // An unusable Location is reported as the redirect status: the
            // URI itself may echo the credentials, so it stays out of the
            // error.
            let Some(next) = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| resolve(&current, location))
            else {
                return Err(fail(GetErrorKind::Status(status.as_u16())));
            };
            redirects += 1;
            log::warn!(
                "HTTP {} redirect from {} to {}",
                status.as_u16(),
                origin(&current),
                origin(&next)
            );
            diverted_to = (host(&next) != original_host).then(|| origin(&next));
            current = next;
            continue;
        }
        // ureq only turns 4xx/5xx into errors; panels answer with custom
        // codes like 884, which must not pass for success either.
        if !status.is_success() {
            return Err(fail(GetErrorKind::Status(status.as_u16())));
        }
        return Ok(response);
    }
}

/// Lowercased host of `url`, if it has one.
fn host(url: &str) -> Option<String> {
    url.parse::<Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_ascii_lowercase))
}

/// `scheme://host[:port]` of `url` — never the path or query, which carry
/// the credentials.
fn origin(url: &str) -> String {
    match url.parse::<Uri>() {
        Ok(uri) => match (uri.scheme_str(), uri.authority()) {
            (Some(scheme), Some(authority)) => format!("{scheme}://{authority}"),
            (None, Some(authority)) => authority.to_string(),
            _ => "<relative URI>".to_owned(),
        },
        Err(_) => "<invalid URI>".to_owned(),
    }
}

/// Resolves a `Location` header against the URL that sent it: absolute,
/// scheme-relative (`//host/…`), absolute-path (`/…`), or relative to the
/// current path. `None` when the result isn't a valid absolute URI.
fn resolve(base: &str, location: &str) -> Option<String> {
    let base = base.parse::<Uri>().ok()?;
    let scheme = base.scheme_str()?;
    let authority = base.authority()?;
    let resolved = if location.contains("://") {
        location.to_owned()
    } else if let Some(rest) = location.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else {
        let path = base.path();
        let dir = &path[..=path.rfind('/').unwrap_or(0)];
        format!("{scheme}://{authority}{dir}{location}")
    };
    let uri = resolved.parse::<Uri>().ok()?;
    (uri.scheme().is_some() && uri.authority().is_some()).then_some(resolved)
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::xtream::http_agent;
    use crate::xtream::test_server::{FAST, ok_head, serve};

    fn redirect_head(location: &str) -> String {
        format!(
            "HTTP/1.1 301 Moved Permanently\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n"
        )
    }

    #[test]
    fn resolves_every_location_form() {
        let base = "http://a.example:8080/dir/page.php?x=1";
        assert_eq!(
            resolve(base, "https://b.example//n").as_deref(),
            Some("https://b.example//n")
        );
        assert_eq!(
            resolve(base, "//b.example/x").as_deref(),
            Some("http://b.example/x")
        );
        assert_eq!(
            resolve(base, "/other?y=2").as_deref(),
            Some("http://a.example:8080/other?y=2")
        );
        assert_eq!(
            resolve(base, "next.php").as_deref(),
            Some("http://a.example:8080/dir/next.php")
        );
        assert_eq!(resolve(base, "http://"), None);
    }

    #[test]
    fn origin_drops_path_and_credentials() {
        assert_eq!(
            origin("https://83.224.65.79//n?username=u&password=secret"),
            "https://83.224.65.79"
        );
        assert_eq!(
            origin("http://h.example:8080/live/u/p/1.ts"),
            "http://h.example:8080"
        );
    }

    #[test]
    fn same_host_redirect_is_followed_without_a_diversion() {
        let body = b"ok".to_vec();
        let (target, target_server) = serve(ok_head(body.len()), vec![body], Duration::ZERO);
        let (start, start_server) = serve(
            redirect_head(&format!("http://127.0.0.1:{target}/final")),
            vec![],
            Duration::ZERO,
        );
        let response = get(
            &http_agent(FAST),
            &format!("http://127.0.0.1:{start}/first"),
            None,
        )
        .unwrap();
        assert!(response.status().is_success());
        drop(response);
        start_server.join().unwrap();
        target_server.join().unwrap();
    }

    #[test]
    fn failure_after_a_cross_host_redirect_names_the_new_host() {
        // Regression: a provider domain diverted to another host whose TLS
        // certificate was rejected surfaced as a bare "certificate expired",
        // with nothing saying the request never reached the provider.
        let (target, target_server) = serve(
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n".into(),
            vec![],
            Duration::ZERO,
        );
        let (start, start_server) = serve(
            redirect_head(&format!(
                "http://localhost:{target}//n?username=u&password=secret"
            )),
            vec![],
            Duration::ZERO,
        );
        let error = get(
            &http_agent(FAST),
            &format!("http://127.0.0.1:{start}/get.php?password=secret"),
            None,
        )
        .unwrap_err();
        assert!(matches!(error.kind, GetErrorKind::Status(503)), "{error:?}");
        assert_eq!(
            error.diverted_to.as_deref(),
            Some(format!("http://localhost:{target}").as_str())
        );
        start_server.join().unwrap();
        target_server.join().unwrap();
    }

    #[test]
    fn redirect_loops_stop_after_the_limit() {
        let mut servers = Vec::new();
        let mut next = None;
        // A chain one longer than the limit, built back to front.
        for _ in 0..=MAX_REDIRECTS {
            let head = match next {
                Some(port) => redirect_head(&format!("http://127.0.0.1:{port}/")),
                None => redirect_head("http://127.0.0.1:9/unreached"),
            };
            let (port, server) = serve(head, vec![], Duration::ZERO);
            servers.push(server);
            next = Some(port);
        }
        let error = get(
            &http_agent(FAST),
            &format!("http://127.0.0.1:{}/", next.unwrap()),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(&error.kind, GetErrorKind::Request(e) if matches!(**e, ureq::Error::TooManyRedirects)),
            "{error:?}"
        );
        for server in servers {
            server.join().unwrap();
        }
    }
}
