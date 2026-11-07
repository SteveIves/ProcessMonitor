//! Running individual checks (one HTTP request or one TCP connection) and describing the result.
//!
//! [`Checker`] is built once per `[[check]]` at start-up (an HTTP check gets its own HTTP client,
//! because timeout, certificate and redirect settings differ per check) and is then run by the
//! monitor loop in `monitor.rs` every round. A check never returns an error: every problem
//! (timeout, refused connection, wrong status, missing text) becomes a failed [`Outcome`] whose
//! `detail` explains what went wrong in words suitable for an alert message.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::config::{CheckConfig, CheckKind, HttpCheck};

/// The result of running a check once.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Whether the check passed.
    pub ok: bool,
    /// Short human-readable description, e.g. "HTTP 200 in 85 ms" or "timed out after 10s".
    pub detail: String,
}

/// A ready-to-run check.
pub struct Checker {
    /// The check's configuration.
    pub config: CheckConfig,
    /// HTTP client, for HTTP checks only.
    client: Option<reqwest::Client>,
}

impl Checker {
    /// Prepares a check, building its HTTP client if it needs one.
    ///
    /// Errors if a configured header name or value is invalid or the HTTP client can't be built.
    pub fn new(config: CheckConfig, user_agent: &str) -> Result<Self> {
        let client = match &config.kind {
            CheckKind::Http(h) => Some(
                build_client(h, config.timeout, user_agent)
                    .with_context(|| format!("setting up check `{}`", config.name))?,
            ),
            CheckKind::Tcp(_) => None,
        };
        Ok(Self { config, client })
    }

    /// A short description of what is checked, for listings: the URL or `host:port`.
    pub fn target(&self) -> &str {
        match &self.config.kind {
            CheckKind::Http(h) => &h.url,
            CheckKind::Tcp(t) => &t.address,
        }
    }

    /// Runs the check once. Never fails; problems are reported in the returned [`Outcome`].
    pub async fn run(&self) -> Outcome {
        let start = Instant::now();
        let timeout = self.config.timeout;
        match &self.config.kind {
            CheckKind::Http(h) => {
                let client = self.client.as_ref().expect("HTTP checks always have a client");
                // The client has its own timeout; this outer one also bounds reading the body.
                match tokio::time::timeout(timeout, run_http(client, h)).await {
                    Ok(Ok(status)) => Outcome { ok: true, detail: format!("HTTP {status} in {}", ms(start)) },
                    // reqwest's own timeout fires at the same moment; report both the same way.
                    Ok(Err(e)) if e == "timed out" => timed_out(timeout),
                    Ok(Err(e)) => Outcome { ok: false, detail: e },
                    Err(_) => timed_out(timeout),
                }
            }
            CheckKind::Tcp(t) => {
                match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&t.address)).await {
                    Ok(Ok(_stream)) => Outcome { ok: true, detail: format!("port open, connected in {}", ms(start)) },
                    Ok(Err(e)) => Outcome { ok: false, detail: describe_io_error(&e) },
                    Err(_) => timed_out(timeout),
                }
            }
        }
    }
}

/// Builds the HTTP client for one check.
fn build_client(h: &HttpCheck, timeout: Duration, user_agent: &str) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    for (k, v) in &h.headers {
        headers.insert(
            HeaderName::from_bytes(k.as_bytes()).with_context(|| format!("bad header name `{k}`"))?,
            HeaderValue::from_str(v).with_context(|| format!("bad value for header `{k}`"))?,
        );
    }
    let redirects =
        if h.follow_redirects { reqwest::redirect::Policy::limited(10) } else { reqwest::redirect::Policy::none() };
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(user_agent)
        .default_headers(headers)
        .redirect(redirects)
        .danger_accept_invalid_certs(h.insecure)
        // A fresh connection each time, so a hung server can't hide behind a pooled connection
        // and every check measures the full connect + TLS + response path.
        .pool_max_idle_per_host(0)
        .build()?)
}

/// Performs the request. Returns the status code on success, or a description of the failure.
async fn run_http(client: &reqwest::Client, h: &HttpCheck) -> Result<u16, String> {
    let method = reqwest::Method::from_bytes(h.method.as_bytes()).map_err(|e| e.to_string())?;
    let resp = client.request(method, &h.url).send().await.map_err(|e| describe_reqwest_error(&e))?;
    let status = resp.status();
    let status_ok = if h.expect_status.is_empty() {
        status.is_success() || status.is_redirection()
    } else {
        h.expect_status.contains(&status.as_u16())
    };
    if !status_ok {
        return Err(format!("HTTP {}", describe_status(status)));
    }
    if let Some(needle) = &h.contains {
        let body = resp.text().await.map_err(|e| format!("error reading page: {}", describe_reqwest_error(&e)))?;
        if !body.contains(needle.as_str()) {
            return Err(format!("HTTP {} but page doesn't contain \"{needle}\"", status.as_u16()));
        }
    }
    Ok(status.as_u16())
}

/// Status code plus reason, with an explanation of Cloudflare's own error codes, which tell you
/// whether Cloudflare could reach your server.
fn describe_status(s: reqwest::StatusCode) -> String {
    let cf = match s.as_u16() {
        520 => Some("Cloudflare: unknown error from origin"),
        521 => Some("Cloudflare: origin refused connection"),
        522 => Some("Cloudflare: origin connection timed out"),
        523 => Some("Cloudflare: origin unreachable"),
        524 => Some("Cloudflare: origin took too long to respond"),
        525 | 526 => Some("Cloudflare: origin TLS error"),
        530 => Some("Cloudflare: tunnel or origin unavailable"),
        _ => None,
    };
    match (cf, s.canonical_reason()) {
        (Some(c), _) => format!("{} ({c})", s.as_u16()),
        (None, Some(r)) => format!("{} {r}", s.as_u16()),
        (None, None) => s.as_u16().to_string(),
    }
}

/// Turns a reqwest error into a short phrase, using the innermost cause (e.g. "connection
/// refused") rather than reqwest's generic "error sending request".
fn describe_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timed out".into();
    }
    let mut source: &dyn std::error::Error = e;
    while let Some(inner) = source.source() {
        source = inner;
    }
    // An OS-level error gets the same short wording as TCP checks, without "(os error 111)".
    let msg = match source.downcast_ref::<std::io::Error>() {
        Some(io) => describe_io_error(io),
        None => source.to_string(),
    };
    if e.is_connect() { format!("can't connect: {msg}") } else { msg }
}

/// Short phrases for the common connection errors.
fn describe_io_error(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => "connection refused".into(),
        TimedOut => "timed out".into(),
        HostUnreachable => "host unreachable".into(),
        NetworkUnreachable => "network unreachable".into(),
        _ => e.to_string(),
    }
}

fn timed_out(t: Duration) -> Outcome {
    Outcome { ok: false, detail: format!("timed out after {}", humantime::format_duration(t)) }
}

/// Milliseconds since `start`, formatted as "123 ms".
fn ms(start: Instant) -> String {
    format!("{} ms", start.elapsed().as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TcpCheck;

    fn tcp_check(address: String) -> Checker {
        Checker::new(
            CheckConfig {
                name: "t".into(),
                alert: true,
                // Generous: Windows retries a refused connection for about 2s before reporting it.
                timeout: Duration::from_secs(10),
                kind: CheckKind::Tcp(TcpCheck { address }),
            },
            "test",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn tcp_open_and_closed_ports() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(tcp_check(addr.to_string()).run().await.ok);
        drop(listener);
        let closed = tcp_check(addr.to_string()).run().await;
        assert!(!closed.ok);
        assert_eq!(closed.detail, "connection refused");
    }

    #[test]
    fn cloudflare_status_explained() {
        let s = describe_status(reqwest::StatusCode::from_u16(522).unwrap());
        assert!(s.contains("timed out"), "{s}");
        assert_eq!(describe_status(reqwest::StatusCode::BAD_GATEWAY), "502 Bad Gateway");
    }
}
