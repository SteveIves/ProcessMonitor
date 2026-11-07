//! Push-notification and chat providers.
//!
//! Each struct here is the settings of one `[[notifier]]` type (its fields are the config keys) and
//! has a `send` method that delivers a [`Message`] using that provider's HTTP API. They are listed
//! in [`super::NotifierKind`] and called by the dispatcher in `notify/mod.rs`. All of them make one
//! (or, for PagerDuty, one per affected check) HTTPS request and return an error describing the
//! provider's reply if it isn't a success.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::json;

use super::{api_url, send_ok, trim_slash};
use crate::config::{EventKind, one_or_many, string_or_number};
use crate::message::{Message, truncate};

/// [ntfy](https://ntfy.sh): free push notifications via the ntfy phone app, on ntfy.sh or your
/// own server. Publishes with ntfy's JSON API (so titles with any characters work).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ntfy {
    /// Server URL. Default `https://ntfy.sh`.
    #[serde(default = "ntfy_server")]
    pub server: String,
    /// Topic to publish to; subscribe to the same topic in the app. Pick something unguessable.
    pub topic: String,
    /// Access token, for protected topics.
    pub token: Option<String>,
    /// Username and password, for protected topics (alternative to `token`).
    pub username: Option<String>,
    pub password: Option<String>,
    /// Priority (1-5) for down and reminder alerts. Default 5 ("urgent": long vibration, pop-over).
    #[serde(default = "five")]
    pub priority: u8,
    /// Extra ntfy tags (emoji short codes or labels).
    #[serde(default)]
    pub tags: Vec<String>,
}

fn ntfy_server() -> String {
    "https://ntfy.sh".into()
}
fn five() -> u8 {
    5
}

impl Ntfy {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let mut tags = self.tags.clone();
        tags.push(
            match msg.event {
                EventKind::Down | EventKind::Reminder => "rotating_light",
                EventKind::Up => "white_check_mark",
                EventKind::Startup | EventKind::Test => "information_source",
            }
            .into(),
        );
        let priority = if msg.is_urgent() { self.priority.clamp(1, 5) } else { 3 };
        let mut req = client.post(api_url(trim_slash(&self.server))).json(&json!({
            "topic": self.topic,
            "title": msg.title,
            "message": msg.body,
            "priority": priority,
            "tags": tags,
        }));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        } else if let Some(u) = &self.username {
            req = req.basic_auth(u, self.password.as_deref());
        }
        send_ok(req).await.map(drop)
    }
}

/// [Pushover](https://pushover.net): push notifications to the Pushover app (one-off purchase).
/// With `priority = 2` ("emergency"), alerts repeat every minute until acknowledged in the app.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pushover {
    /// Application API token (create an application in your Pushover dashboard).
    pub token: String,
    /// Your user key (or a group key).
    pub user: String,
    /// Send to this device only.
    pub device: Option<String>,
    /// Notification sound for down/reminder alerts, e.g. "siren".
    pub sound: Option<String>,
    /// Priority (-2 to 2) for down/reminder alerts. Default 1 (high: bypasses quiet hours).
    #[serde(default = "one_i8")]
    pub priority: i8,
}

fn one_i8() -> i8 {
    1
}

impl Pushover {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let priority = if msg.is_urgent() { self.priority.clamp(-2, 2) } else { 0 };
        let mut form: Vec<(&str, String)> = vec![
            ("token", self.token.clone()),
            ("user", self.user.clone()),
            ("title", truncate(&msg.title, 250)),
            ("message", truncate(&msg.body, 1024)),
            ("priority", priority.to_string()),
        ];
        if priority == 2 {
            // Emergency priority requires a retry interval and an expiry (seconds).
            form.push(("retry", "60".into()));
            form.push(("expire", "3600".into()));
        }
        if let Some(d) = &self.device {
            form.push(("device", d.clone()));
        }
        if let (Some(s), true) = (&self.sound, msg.is_urgent()) {
            form.push(("sound", s.clone()));
        }
        send_ok(client.post(api_url("https://api.pushover.net/1/messages.json")).form(&form)).await.map(drop)
    }
}

/// [Gotify](https://gotify.net): self-hosted push server with an Android app.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gotify {
    /// Server URL, e.g. `https://gotify.example.com`.
    pub server: String,
    /// Application token.
    pub token: String,
    /// Priority for down/reminder alerts. Default 8 (Gotify shows 8+ as high priority).
    #[serde(default = "eight")]
    pub priority: u8,
}

fn eight() -> u8 {
    8
}

impl Gotify {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let priority = if msg.is_urgent() { self.priority } else { 5 };
        let req = client
            .post(api_url(&format!("{}/message", trim_slash(&self.server))))
            .header("X-Gotify-Key", &self.token)
            .json(&json!({ "title": msg.title, "message": msg.body, "priority": priority }));
        send_ok(req).await.map(drop)
    }
}

/// [Telegram](https://core.telegram.org/bots): messages from a bot you create with @BotFather.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    /// Bot token from @BotFather.
    pub bot_token: String,
    /// Chat to send to: your user id, a group id (negative) or "@channelname".
    #[serde(deserialize_with = "string_or_number")]
    pub chat_id: String,
}

impl Telegram {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let url = api_url(&format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token));
        let req = client.post(url).json(&json!({
            "chat_id": self.chat_id,
            "text": truncate(&msg.title_and_body(), 4096),
            "disable_notification": !msg.is_urgent() && msg.event != EventKind::Up,
        }));
        send_ok(req).await.map(drop)
    }
}

/// [Discord](https://discord.com): a channel webhook (Channel settings > Integrations > Webhooks).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discord {
    /// The webhook URL.
    pub webhook_url: String,
    /// Name the messages appear under. Default: the webhook's own name.
    pub username: Option<String>,
}

impl Discord {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let mut body = json!({ "content": truncate(&format!("**{}**\n{}", msg.title, msg.body), 2000) });
        if let Some(u) = &self.username {
            body["username"] = json!(u);
        }
        send_ok(client.post(api_url(&self.webhook_url)).json(&body)).await.map(drop)
    }
}

/// [Slack](https://api.slack.com/messaging/webhooks) incoming webhook. Also works with Mattermost
/// and Rocket.Chat incoming webhooks, which accept the same format.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slack {
    /// The incoming-webhook URL.
    pub webhook_url: String,
}

impl Slack {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let body = json!({ "text": format!("*{}*\n{}", msg.title, msg.body) });
        send_ok(client.post(api_url(&self.webhook_url)).json(&body)).await.map(drop)
    }
}

/// Microsoft Teams, through a Workflows webhook ("Post to a channel when a webhook request is
/// received"). The message is sent as an Adaptive Card, the format those workflows expect.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Teams {
    /// The workflow's HTTP POST URL.
    pub webhook_url: String,
}

impl Teams {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let mut blocks = vec![json!({
            "type": "TextBlock", "text": msg.title, "weight": "Bolder", "size": "Medium", "wrap": true,
            "color": if msg.is_urgent() { "Attention" } else { "Default" },
        })];
        // One TextBlock per paragraph keeps the line breaks, which Teams otherwise collapses.
        for para in msg.body.split("\n\n") {
            blocks.push(json!({ "type": "TextBlock", "text": para.replace('\n', "\n\n"), "wrap": true }));
        }
        let body = json!({
            "type": "message",
            "attachments": [{
                "contentType": "application/vnd.microsoft.card.adaptive",
                "content": {
                    "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                    "type": "AdaptiveCard",
                    "version": "1.4",
                    "body": blocks,
                },
            }],
        });
        send_ok(client.post(api_url(&self.webhook_url)).json(&body)).await.map(drop)
    }
}

/// [Google Chat](https://developers.google.com/workspace/chat/quickstart/webhooks) space webhook.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleChat {
    /// The webhook URL (Space > Apps & integrations > Webhooks).
    pub webhook_url: String,
}

impl GoogleChat {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let body = json!({ "text": format!("*{}*\n{}", msg.title, msg.body) });
        send_ok(client.post(api_url(&self.webhook_url)).json(&body)).await.map(drop)
    }
}

/// [Matrix](https://matrix.org) room message (e.g. read in the Element app).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matrix {
    /// Homeserver URL, e.g. `https://matrix.org`.
    pub homeserver: String,
    /// Access token of the account that sends the messages (ideally a separate bot account).
    pub access_token: String,
    /// Room id, e.g. `!abcdefg:matrix.org` (Room settings > Advanced). The account must be joined.
    pub room_id: String,
}

/// Counter that makes Matrix transaction ids unique within one run.
static MATRIX_TXN: AtomicU64 = AtomicU64::new(0);

impl Matrix {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        // Transaction ids must be unique per access token; time + counter is unique enough.
        let txn = format!(
            "process-monitor-{}-{}",
            chrono::Utc::now().timestamp_millis(),
            MATRIX_TXN.fetch_add(1, Ordering::Relaxed)
        );
        let mut url = reqwest::Url::parse(&api_url(trim_slash(&self.homeserver)))?;
        url.path_segments_mut().map_err(|_| anyhow::anyhow!("bad homeserver URL"))?.pop_if_empty().extend([
            "_matrix",
            "client",
            "v3",
            "rooms",
            &self.room_id,
            "send",
            "m.room.message",
            &txn,
        ]);
        let req = client
            .put(url)
            .bearer_auth(&self.access_token)
            .json(&json!({ "msgtype": "m.text", "body": msg.title_and_body() }));
        send_ok(req).await.map(drop)
    }
}

/// [Pushbullet](https://www.pushbullet.com) note push.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pushbullet {
    /// Access token (Settings > Account > Create access token).
    pub access_token: String,
    /// Send to this device only (its "iden"). Default: all your devices.
    pub device_iden: Option<String>,
}

impl Pushbullet {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let mut body = json!({ "type": "note", "title": msg.title, "body": msg.body });
        if let Some(d) = &self.device_iden {
            body["device_iden"] = json!(d);
        }
        let req = client
            .post(api_url("https://api.pushbullet.com/v2/pushes"))
            .header("Access-Token", &self.access_token)
            .json(&body);
        send_ok(req).await.map(drop)
    }
}

/// Signal messages through a [signal-cli-rest-api](https://github.com/bbernhard/signal-cli-rest-api)
/// server that you run (Signal has no official API).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    /// URL of the signal-cli-rest-api server, e.g. `http://localhost:8080`.
    pub api_url: String,
    /// The registered Signal number that sends, e.g. "+15551234567".
    #[serde(deserialize_with = "string_or_number")]
    pub number: String,
    /// Recipient number(s) or group id(s).
    #[serde(deserialize_with = "one_or_many")]
    pub recipients: Vec<String>,
}

impl Signal {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let req = client.post(api_url(&format!("{}/v2/send", trim_slash(&self.api_url)))).json(&json!({
            "message": msg.title_and_body(),
            "number": self.number,
            "recipients": self.recipients,
        }));
        send_ok(req).await.map(drop)
    }
}

/// [Bark](https://bark.day.app): push notifications for iPhone.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bark {
    /// Server URL. Default `https://api.day.app`.
    #[serde(default = "bark_server")]
    pub server: String,
    /// Your device key (shown in the Bark app).
    pub device_key: String,
    /// Interruption level for down/reminder alerts: "active", "timeSensitive" (default, breaks
    /// through Focus), "passive" or "critical" (sounds even when muted).
    #[serde(default = "bark_level")]
    pub level: String,
    /// Sound name for down/reminder alerts.
    pub sound: Option<String>,
}

fn bark_server() -> String {
    "https://api.day.app".into()
}
fn bark_level() -> String {
    "timeSensitive".into()
}

impl Bark {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let mut body = json!({
            "device_key": self.device_key,
            "title": msg.title,
            "body": msg.body,
            "group": msg.monitor,
            "level": if msg.is_urgent() { self.level.as_str() } else { "active" },
        });
        if let (Some(s), true) = (&self.sound, msg.is_urgent()) {
            body["sound"] = json!(s);
        }
        send_ok(client.post(api_url(&format!("{}/push", trim_slash(&self.server)))).json(&body)).await.map(drop)
    }
}

/// [PagerDuty](https://developer.pagerduty.com/docs/events-api-v2/overview/) Events API v2. Each
/// check that goes down opens (triggers) an incident, which is resolved when it comes back up, so
/// PagerDuty's own phone call / SMS / app escalation rules apply.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagerDuty {
    /// Integration (routing) key of an "Events API v2" integration on a PagerDuty service.
    pub routing_key: String,
}

impl PagerDuty {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let url = api_url("https://events.pagerduty.com/v2/enqueue");
        let event = |action: &str, dedup: String, severity: &str| {
            json!({
                "routing_key": self.routing_key,
                "event_action": action,
                "dedup_key": dedup,
                "payload": {
                    "summary": truncate(&msg.title, 1024),
                    "source": msg.monitor,
                    "severity": severity,
                    "custom_details": { "details": msg.body },
                },
            })
        };
        match msg.event {
            EventKind::Down | EventKind::Reminder | EventKind::Up => {
                let action = if msg.event == EventKind::Up { "resolve" } else { "trigger" };
                for check in &msg.checks {
                    let dedup = format!("{}:{check}", msg.monitor);
                    send_ok(client.post(&url).json(&event(action, dedup, "critical"))).await?;
                }
            }
            EventKind::Test | EventKind::Startup => {
                // Open and immediately resolve an informational event, so a test shows up
                // without leaving an incident open.
                let dedup = format!("{}:{}", msg.monitor, msg.event.as_str());
                send_ok(client.post(&url).json(&event("trigger", dedup.clone(), "info"))).await?;
                send_ok(client.post(&url).json(&event("resolve", dedup, "info"))).await?;
            }
        }
        Ok(())
    }
}

/// Generic webhook, for anything not listed (Home Assistant, n8n, IFTTT, Apprise, your own script).
///
/// Without `body`, POSTs JSON: `{"event", "title", "message", "short", "checks", "monitor"}`.
/// With `body`, sends that text with `{event}`, `{title}`, `{message}`, `{short}` and `{monitor}`
/// replaced (JSON-escaped when the content type is JSON, so a template like
/// `{"text": "{title}"}` stays valid).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// URL to call.
    pub url: String,
    /// HTTP method. Default POST.
    #[serde(default = "post")]
    pub method: String,
    /// Extra headers, e.g. `{ Authorization = "Bearer ..." }`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Body template (see above). Default: the JSON object described above.
    pub body: Option<String>,
    /// Content-Type of a templated body. Default "application/json".
    #[serde(default = "json_type")]
    pub content_type: String,
}

fn post() -> String {
    "POST".into()
}
fn json_type() -> String {
    "application/json".into()
}

impl Webhook {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let Ok(method) = reqwest::Method::from_bytes(self.method.as_bytes()) else {
            bail!("bad method `{}`", self.method);
        };
        let mut req = client.request(method, api_url(&self.url));
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        req = match &self.body {
            None => req.json(&json!({
                "event": msg.event.as_str(),
                "title": msg.title,
                "message": msg.body,
                "short": msg.short,
                "checks": msg.checks,
                "monitor": msg.monitor,
            })),
            Some(template) => {
                let is_json = self.content_type.contains("json");
                req.header(reqwest::header::CONTENT_TYPE, &self.content_type)
                    .body(fill_template(template, msg, is_json))
            }
        };
        send_ok(req).await.map(drop)
    }
}

/// Replaces the `{placeholders}` of a webhook body template.
fn fill_template(template: &str, msg: &Message, json_escape: bool) -> String {
    let esc = |s: &str| -> String {
        if json_escape {
            // serde_json produces a quoted string; drop the quotes to splice it into the template.
            let q = serde_json::to_string(s).unwrap_or_default();
            q[1..q.len() - 1].to_string()
        } else {
            s.to_string()
        }
    };
    template
        .replace("{event}", msg.event.as_str())
        .replace("{title}", &esc(&msg.title))
        .replace("{message}", &esc(&msg.body))
        .replace("{short}", &esc(&msg.short))
        .replace("{monitor}", &esc(&msg.monitor))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::exchange;

    #[tokio::test]
    async fn ntfy() {
        let (r, req) = exchange("type='ntfy'\ntopic='alerts'\ntoken='tk'", 200, "{}").await;
        r.unwrap();
        assert_eq!(req.line(), "POST / HTTP/1.1");
        assert_eq!(req.header("authorization"), Some("Bearer tk"));
        let j = req.json();
        assert_eq!(j["topic"], "alerts");
        assert_eq!(j["title"], "DOWN: Gallery");
        assert_eq!(j["priority"], 5);
    }

    #[tokio::test]
    async fn pushover() {
        let (r, req) = exchange("type='pushover'\ntoken='app'\nuser='me'\npriority=2", 200, "{}").await;
        r.unwrap();
        assert_eq!(req.line(), "POST /1/messages.json HTTP/1.1");
        assert!(req.body.contains("token=app") && req.body.contains("priority=2") && req.body.contains("retry=60"));
    }

    #[tokio::test]
    async fn telegram_token_in_path_and_hidden_in_errors() {
        let (r, req) = exchange("type='telegram'\nbot_token='123:SECRET'\nchat_id=-100", 401, "{\"ok\":false}").await;
        assert_eq!(req.line(), "POST /bot123:SECRET/sendMessage HTTP/1.1");
        assert_eq!(req.json()["chat_id"], "-100");
        let err = format!("{:#}", r.unwrap_err());
        assert!(!err.contains("SECRET"), "{err}");
        assert!(err.contains("401"), "{err}");
    }

    #[tokio::test]
    async fn matrix_room_id_is_escaped() {
        let (r, req) =
            exchange("type='matrix'\nhomeserver='https://m.org'\naccess_token='t'\nroom_id='!abc:m.org'", 200, "{}")
                .await;
        r.unwrap();
        assert!(
            req.line().starts_with("PUT /_matrix/client/v3/rooms/!abc:m.org/send/m.room.message/process-monitor-"),
            "{}",
            req.line()
        );
        assert_eq!(req.json()["msgtype"], "m.text");
    }

    #[tokio::test]
    async fn teams_adaptive_card() {
        let (r, req) = exchange("type='teams'\nwebhook_url='https://x.example/wf'", 202, "").await;
        r.unwrap();
        let j = req.json();
        assert_eq!(j["attachments"][0]["content"]["type"], "AdaptiveCard");
        assert_eq!(j["attachments"][0]["content"]["body"][0]["text"], "DOWN: Gallery");
    }

    #[tokio::test]
    async fn pagerduty_triggers_per_check() {
        let (r, req) = exchange("type='pagerduty'\nrouting_key='rk'", 202, "{}").await;
        r.unwrap();
        let j = req.json();
        assert_eq!(j["event_action"], "trigger");
        assert_eq!(j["dedup_key"], "pi:Gallery");
    }

    #[tokio::test]
    async fn webhook_template_is_json_escaped() {
        let toml = "type='webhook'\nurl='https://h.example/hook'\nbody='{\"text\": \"{title}\\n{message}\"}'";
        let (r, req) = exchange(toml, 200, "").await;
        r.unwrap();
        let j = req.json();
        assert!(j["text"].as_str().unwrap().starts_with("DOWN: Gallery\nGallery: timed out"));
    }

    #[tokio::test]
    async fn webhook_default_json() {
        let (r, req) = exchange("type='webhook'\nurl='https://h.example/hook'\nheaders={X-Key='k'}", 200, "").await;
        r.unwrap();
        assert_eq!(req.header("x-key"), Some("k"));
        assert_eq!(req.json()["event"], "down");
    }

    #[tokio::test]
    async fn signal_and_bark_and_gotify() {
        let (r, req) =
            exchange("type='signal'\napi_url='http://s:8080/'\nnumber='+1555'\nrecipients='+1666'", 201, "{}").await;
        r.unwrap();
        assert_eq!(req.line(), "POST /v2/send HTTP/1.1");
        assert_eq!(req.json()["recipients"][0], "+1666");

        let (r, req) = exchange("type='bark'\ndevice_key='dk'", 200, "{}").await;
        r.unwrap();
        assert_eq!(req.line(), "POST /push HTTP/1.1");
        assert_eq!(req.json()["level"], "timeSensitive");

        let (r, req) = exchange("type='gotify'\nserver='https://g'\ntoken='gt'", 200, "{}").await;
        r.unwrap();
        assert_eq!(req.header("x-gotify-key"), Some("gt"));
        assert_eq!(req.json()["priority"], 8);
    }
}
