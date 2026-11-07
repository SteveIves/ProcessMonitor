//! Sending alerts: the list of supported providers and the dispatcher that delivers messages.
//!
//! Each `[[notifier]]` in the config becomes one [`NotifierConfig`] whose `kind` is a
//! [`NotifierKind`] variant selected by its `type` key. The providers are grouped by channel:
//!
//! - `push.rs`: push and chat services (ntfy, Pushover, Gotify, Telegram, Discord, Slack, Teams,
//!   Google Chat, Matrix, Pushbullet, Signal, Bark, PagerDuty, generic webhook);
//! - `sms.rs`: SMS gateways (Twilio, Vonage, Plivo, Telnyx, Sinch, ClickSend, Infobip, TextBelt);
//! - `email.rs`: email (SMTP, SendGrid, Mailgun, Postmark, Resend, Brevo).
//!
//! The monitor loop calls [`Dispatcher::send`], which delivers a message to every notifier that
//! subscribes to its event in background tasks, retrying failures for several minutes (the
//! monitor's own internet connection may be what's failing). The `test-notify` command uses
//! [`Dispatcher::send_now`], which sends once and reports each result.

mod email;
mod push;
mod sms;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::NotifierConfig;
use crate::message::{Message, truncate};

/// Every supported provider. The variant is chosen by the notifier's `type` key.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotifierKind {
    // Push and chat
    Ntfy(push::Ntfy),
    Pushover(push::Pushover),
    Gotify(push::Gotify),
    Telegram(push::Telegram),
    Discord(push::Discord),
    Slack(push::Slack),
    Teams(push::Teams),
    GoogleChat(push::GoogleChat),
    Matrix(push::Matrix),
    Pushbullet(push::Pushbullet),
    Signal(push::Signal),
    Bark(push::Bark),
    Pagerduty(push::PagerDuty),
    Webhook(push::Webhook),
    // SMS
    Twilio(sms::Twilio),
    Vonage(sms::Vonage),
    Plivo(sms::Plivo),
    Telnyx(sms::Telnyx),
    Sinch(sms::Sinch),
    Clicksend(sms::ClickSend),
    Infobip(sms::Infobip),
    Textbelt(sms::TextBelt),
    // Email
    Smtp(email::Smtp),
    Sendgrid(email::SendGrid),
    Mailgun(email::Mailgun),
    Postmark(email::Postmark),
    Resend(email::Resend),
    Brevo(email::Brevo),
}

impl NotifierKind {
    /// The `type` value of this provider, as written in the config file.
    pub fn type_name(&self) -> &'static str {
        use NotifierKind::*;
        match self {
            Ntfy(_) => "ntfy",
            Pushover(_) => "pushover",
            Gotify(_) => "gotify",
            Telegram(_) => "telegram",
            Discord(_) => "discord",
            Slack(_) => "slack",
            Teams(_) => "teams",
            GoogleChat(_) => "google_chat",
            Matrix(_) => "matrix",
            Pushbullet(_) => "pushbullet",
            Signal(_) => "signal",
            Bark(_) => "bark",
            Pagerduty(_) => "pagerduty",
            Webhook(_) => "webhook",
            Twilio(_) => "twilio",
            Vonage(_) => "vonage",
            Plivo(_) => "plivo",
            Telnyx(_) => "telnyx",
            Sinch(_) => "sinch",
            Clicksend(_) => "clicksend",
            Infobip(_) => "infobip",
            Textbelt(_) => "textbelt",
            Smtp(_) => "smtp",
            Sendgrid(_) => "sendgrid",
            Mailgun(_) => "mailgun",
            Postmark(_) => "postmark",
            Resend(_) => "resend",
            Brevo(_) => "brevo",
        }
    }

    /// Delivers one message through this provider. Errors describe what the provider said.
    async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        use NotifierKind::*;
        match self {
            Ntfy(n) => n.send(client, msg).await,
            Pushover(n) => n.send(client, msg).await,
            Gotify(n) => n.send(client, msg).await,
            Telegram(n) => n.send(client, msg).await,
            Discord(n) => n.send(client, msg).await,
            Slack(n) => n.send(client, msg).await,
            Teams(n) => n.send(client, msg).await,
            GoogleChat(n) => n.send(client, msg).await,
            Matrix(n) => n.send(client, msg).await,
            Pushbullet(n) => n.send(client, msg).await,
            Signal(n) => n.send(client, msg).await,
            Bark(n) => n.send(client, msg).await,
            Pagerduty(n) => n.send(client, msg).await,
            Webhook(n) => n.send(client, msg).await,
            Twilio(n) => n.send(client, msg).await,
            Vonage(n) => n.send(client, msg).await,
            Plivo(n) => n.send(client, msg).await,
            Telnyx(n) => n.send(client, msg).await,
            Sinch(n) => n.send(client, msg).await,
            Clicksend(n) => n.send(client, msg).await,
            Infobip(n) => n.send(client, msg).await,
            Textbelt(n) => n.send(client, msg).await,
            Smtp(n) => n.send(msg).await,
            Sendgrid(n) => n.send(client, msg).await,
            Mailgun(n) => n.send(client, msg).await,
            Postmark(n) => n.send(client, msg).await,
            Resend(n) => n.send(client, msg).await,
            Brevo(n) => n.send(client, msg).await,
        }
    }
}

/// Delay before each retry of a failed delivery (about 9 minutes in total), so a short internet
/// outage on the monitor's side doesn't lose the alert.
const RETRY_DELAYS: [u64; 6] = [5, 15, 30, 60, 120, 300];

/// Delivers messages to the configured notifiers.
pub struct Dispatcher {
    notifiers: Vec<Arc<NotifierConfig>>,
    client: reqwest::Client,
    /// Deliveries currently in progress (including ones waiting to retry).
    in_flight: Arc<AtomicUsize>,
}

impl Dispatcher {
    /// Creates the dispatcher and the HTTP client shared by all HTTP-based providers.
    pub fn new(notifiers: Vec<NotifierConfig>, user_agent: &str) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).user_agent(user_agent).build()?;
        Ok(Self { notifiers: notifiers.into_iter().map(Arc::new).collect(), client, in_flight: Default::default() })
    }

    /// Number of configured notifiers.
    pub fn len(&self) -> usize {
        self.notifiers.len()
    }

    /// Sends `msg` to every notifier subscribed to its event, in the background, with retries.
    /// Returns immediately; failures are logged.
    pub fn send(&self, msg: Message) {
        let msg = Arc::new(msg);
        for n in self.notifiers.iter().filter(|n| n.events.contains(&msg.event)) {
            let (n, msg, client, in_flight) = (n.clone(), msg.clone(), self.client.clone(), self.in_flight.clone());
            in_flight.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                deliver_with_retries(&n, &client, &msg).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }

    /// Sends `msg` once to each notifier (or only the one named `only`), ignoring event filters,
    /// and returns each notifier's name and result. Used by `test-notify`.
    ///
    /// Errors if `only` names no configured notifier.
    pub async fn send_now(&self, msg: &Message, only: Option<&str>) -> Result<Vec<(String, Result<()>)>> {
        let selected: Vec<_> = self.notifiers.iter().filter(|n| only.is_none_or(|o| n.name == o)).collect();
        if selected.is_empty() {
            match only {
                Some(o) => bail!("no notifier named `{o}`"),
                None => bail!("no [[notifier]] entries in the config"),
            }
        }
        let results = futures::future::join_all(selected.iter().map(|n| n.kind.send(&self.client, msg))).await;
        Ok(selected.iter().map(|n| n.name.clone()).zip(results).collect())
    }

    /// Waits until no deliveries are in progress, or `limit` has passed.
    pub async fn wait_idle(&self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        while self.in_flight.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Tries to deliver `msg` through `n`, retrying with increasing delays.
async fn deliver_with_retries(n: &NotifierConfig, client: &reqwest::Client, msg: &Message) {
    // Each attempt is followed by the next delay, or by `None` after the last attempt.
    let delays = RETRY_DELAYS.iter().map(Some).chain([None]);
    for delay in delays {
        match (n.kind.send(client, msg).await, delay) {
            (Ok(()), _) => {
                info!("sent \"{}\" via {}", msg.title, n.name);
                return;
            }
            (Err(e), Some(&secs)) => {
                warn!("sending via {} failed (will retry in {secs}s): {e:#}", n.name);
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
            (Err(e), None) => warn!("giving up sending \"{}\" via {}: {e:#}", msg.title, n.name),
        }
    }
}

// ----- Helpers shared by the provider modules -----

/// Sends a request and fails unless the response status is 2xx. The error includes the start of
/// the response body (providers explain problems there) but never the request URL, which for some
/// providers (Telegram, webhooks) contains the secret token.
pub(crate) async fn send_ok(req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let resp = req.send().await.map_err(|e| anyhow::anyhow!(e.without_url()))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    bail!("server replied HTTP {status}: {}", truncate(body.trim(), 300))
}

/// Like [`send_ok`], then parses the JSON reply (for providers that report errors inside a 200).
pub(crate) async fn send_json(req: reqwest::RequestBuilder) -> Result<serde_json::Value> {
    let resp = send_ok(req).await?;
    resp.json().await.map_err(|e| anyhow::anyhow!("unreadable reply: {}", e.without_url()))
}

/// Returns a provider's fixed API URL. In tests, the scheme and host are replaced by the mock
/// server's address (set with [`test_support::redirect_to`]) so requests can be inspected.
pub(crate) fn api_url(url: &str) -> String {
    #[cfg(test)]
    if let Some(base) = test_support::BASE.with(|b| b.borrow().clone()) {
        let path_start = url.find("://").map(|i| i + 3).and_then(|i| url[i..].find('/').map(|j| i + j));
        return format!("{base}{}", path_start.map(|p| &url[p..]).unwrap_or("/"));
    }
    url.to_string()
}

/// Removes a trailing slash so `{server}/path` doesn't produce `//path`.
pub(crate) fn trim_slash(s: &str) -> &str {
    s.trim_end_matches('/')
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A one-request HTTP server for checking what a provider sends.

    use std::cell::RefCell;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    thread_local! {
        /// Base URL that [`super::api_url`] substitutes in tests (per thread: tokio tests are
        /// single-threaded by default, so this doesn't leak between tests).
        pub static BASE: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// What the mock server received.
    #[derive(Debug)]
    pub struct Captured {
        /// Request line and headers, e.g. "POST /path HTTP/1.1\r\nhost: ...".
        pub head: String,
        /// Request body.
        pub body: String,
    }

    impl Captured {
        /// The request line, e.g. "POST /v2/send HTTP/1.1".
        pub fn line(&self) -> &str {
            self.head.lines().next().unwrap_or("")
        }
        /// Value of a header (case-insensitive name).
        pub fn header(&self, name: &str) -> Option<&str> {
            self.head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim())
            })
        }
        /// Body parsed as JSON.
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).expect("body is JSON")
        }
    }

    /// Starts a server that answers one request with `status` and `reply`, points
    /// [`super::api_url`] at it, and returns its base URL and a handle yielding the request.
    pub async fn mock(status: u16, reply: &str) -> (String, tokio::task::JoinHandle<Captured>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        BASE.with(|b| *b.borrow_mut() = Some(base.clone()));
        let reply = reply.to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body_start, len) = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0usize);
                    break (head, pos + 4, len);
                }
            };
            while buf.len() < body_start + len {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8_lossy(&buf[body_start..body_start + len]).to_string();
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            Captured { head, body }
        });
        (base, handle)
    }

    /// A sample "down" message.
    pub fn message() -> crate::message::Message {
        crate::message::Message {
            event: crate::config::EventKind::Down,
            title: "DOWN: Gallery".into(),
            body: "Gallery: timed out after 10s\n\nStatus of all checks: ...".into(),
            short: "DOWN: Gallery: timed out after 10s. Others: ssh ok. pi 14:03".into(),
            checks: vec!["Gallery".into()],
            monitor: "pi".into(),
        }
    }

    /// Parses a provider config from TOML text.
    pub fn kind(toml_text: &str) -> super::NotifierKind {
        let v: toml::Value = toml::from_str(toml_text).unwrap();
        serde::Deserialize::deserialize(v).unwrap()
    }

    /// Sends the sample message through the provider described by `toml_text` to a mock server
    /// replying `status`/`reply`, and returns the send result and the captured request.
    pub async fn exchange(toml_text: &str, status: u16, reply: &str) -> (anyhow::Result<()>, Captured) {
        let (_base, handle) = mock(status, reply).await;
        let client = reqwest::Client::new();
        let result = kind(toml_text).send(&client, &message()).await;
        (result, handle.await.unwrap())
    }
}
