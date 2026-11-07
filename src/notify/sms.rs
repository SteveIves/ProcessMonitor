//! SMS gateway providers.
//!
//! Each struct is the settings of one `[[notifier]]` type and has a `send` method that texts the
//! message's one-line `short` rendering (cut to `max_length`, 160 characters by default, which is
//! one SMS) to every number in `to`. Numbers should be in international format, e.g.
//! "+15551234567". Listed in [`super::NotifierKind`] and called by the dispatcher in
//! `notify/mod.rs`. Several gateways answer HTTP 200 even when a message was rejected, so their
//! replies are inspected and turned into errors.
//!
//! For a free option without an SMS account, an email notifier sent to your carrier's
//! email-to-SMS address (e.g. `5551234567@tmomail.net`) with `format = "short"` also works.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::json;

use super::{api_url, send_json, send_ok, trim_slash};
use crate::config::{one_or_many, string_or_number};
use crate::message::{Message, truncate};

fn default_max() -> usize {
    160
}

/// The text to send: the message's SMS rendering, cut to `max` characters.
fn text(msg: &Message, max: usize) -> String {
    truncate(&msg.short, max)
}

/// [Twilio](https://www.twilio.com/docs/messaging/api/message-resource).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Twilio {
    /// Account SID ("AC...").
    pub account_sid: String,
    /// Auth token.
    pub auth_token: String,
    /// Your Twilio phone number. Either this or `messaging_service_sid` is needed.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub from: Option<String>,
    /// Messaging Service SID ("MG..."), instead of `from`.
    pub messaging_service_sid: Option<String>,
    /// Recipient number(s).
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    /// Maximum characters per message. Default 160.
    #[serde(default = "default_max")]
    pub max_length: usize,
}

impl Twilio {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let url = api_url(&format!("https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json", self.account_sid));
        let sender = match (&self.from, &self.messaging_service_sid) {
            (_, Some(ms)) => ("MessagingServiceSid", ms.clone()),
            (Some(f), None) => ("From", f.clone()),
            (None, None) => bail!("twilio needs `from` or `messaging_service_sid`"),
        };
        for to in &self.to {
            let form = [("To", to.clone()), sender.clone(), ("Body", text(msg, self.max_length))];
            send_ok(client.post(&url).basic_auth(&self.account_sid, Some(&self.auth_token)).form(&form)).await?;
        }
        Ok(())
    }
}

/// [Vonage](https://developer.vonage.com/en/messaging/sms/overview) (formerly Nexmo) SMS API.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vonage {
    pub api_key: String,
    pub api_secret: String,
    /// Sender number or name (sender names aren't allowed in some countries, including the US).
    #[serde(deserialize_with = "string_or_number")]
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default = "default_max")]
    pub max_length: usize,
}

impl Vonage {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        for to in &self.to {
            let form = [
                ("api_key", self.api_key.clone()),
                ("api_secret", self.api_secret.clone()),
                ("from", self.from.clone()),
                // Vonage wants the number without the leading "+".
                ("to", to.trim_start_matches('+').to_string()),
                ("text", text(msg, self.max_length)),
            ];
            let reply = send_json(client.post(api_url("https://rest.nexmo.com/sms/json")).form(&form)).await?;
            // Replies 200 even on failure; each message part has a status, "0" meaning success.
            for part in reply["messages"].as_array().into_iter().flatten() {
                if part["status"] != "0" {
                    bail!("Vonage rejected the message: {}", part["error-text"].as_str().unwrap_or("unknown error"));
                }
            }
        }
        Ok(())
    }
}

/// [Plivo](https://www.plivo.com/docs/messaging/api/message).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plivo {
    pub auth_id: String,
    pub auth_token: String,
    #[serde(deserialize_with = "string_or_number")]
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default = "default_max")]
    pub max_length: usize,
}

impl Plivo {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let url = api_url(&format!("https://api.plivo.com/v1/Account/{}/Message/", self.auth_id));
        // Plivo sends to several numbers in one request, separated by "<".
        let body = json!({ "src": self.from, "dst": self.to.join("<"), "text": text(msg, self.max_length) });
        send_ok(client.post(url).basic_auth(&self.auth_id, Some(&self.auth_token)).json(&body)).await.map(drop)
    }
}

/// [Telnyx](https://developers.telnyx.com/api/messaging/send-message).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telnyx {
    /// API key ("KEY...").
    pub api_key: String,
    /// Your Telnyx number. Either this or `messaging_profile_id` is needed.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub from: Option<String>,
    pub messaging_profile_id: Option<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default = "default_max")]
    pub max_length: usize,
}

impl Telnyx {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        if self.from.is_none() && self.messaging_profile_id.is_none() {
            bail!("telnyx needs `from` or `messaging_profile_id`");
        }
        for to in &self.to {
            let mut body = json!({ "to": to, "text": text(msg, self.max_length) });
            if let Some(f) = &self.from {
                body["from"] = json!(f);
            }
            if let Some(p) = &self.messaging_profile_id {
                body["messaging_profile_id"] = json!(p);
            }
            let req = client.post(api_url("https://api.telnyx.com/v2/messages")).bearer_auth(&self.api_key).json(&body);
            send_ok(req).await?;
        }
        Ok(())
    }
}

/// [Sinch](https://developers.sinch.com/docs/sms/api-reference/) SMS REST API.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sinch {
    pub service_plan_id: String,
    pub api_token: String,
    #[serde(deserialize_with = "string_or_number")]
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    /// Region of your account: "us" (default), "eu", "au", "br" or "ca".
    #[serde(default = "us")]
    pub region: String,
    #[serde(default = "default_max")]
    pub max_length: usize,
}

fn us() -> String {
    "us".into()
}

impl Sinch {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let url =
            api_url(&format!("https://{}.sms.api.sinch.com/xms/v1/{}/batches", self.region, self.service_plan_id));
        let body = json!({ "from": self.from, "to": self.to, "body": text(msg, self.max_length) });
        send_ok(client.post(url).bearer_auth(&self.api_token).json(&body)).await.map(drop)
    }
}

/// [ClickSend](https://developers.clicksend.com/docs/messaging/sms/).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickSend {
    /// Your ClickSend username.
    pub username: String,
    /// API key (Dashboard > API Credentials).
    pub api_key: String,
    /// Sender number or name. Default: ClickSend's shared number.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub from: Option<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default = "default_max")]
    pub max_length: usize,
}

impl ClickSend {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let body = text(msg, self.max_length);
        let messages: Vec<_> = self
            .to
            .iter()
            .map(|to| {
                let mut m = json!({ "source": "process-monitor", "body": body, "to": to });
                if let Some(f) = &self.from {
                    m["from"] = json!(f);
                }
                m
            })
            .collect();
        let req = client
            .post(api_url("https://rest.clicksend.com/v3/sms/send"))
            .basic_auth(&self.username, Some(&self.api_key))
            .json(&json!({ "messages": messages }));
        let reply = send_json(req).await?;
        // Each message has its own status, "SUCCESS" when accepted.
        for m in reply["data"]["messages"].as_array().into_iter().flatten() {
            if m["status"] != "SUCCESS" {
                bail!("ClickSend rejected the message to {}: {}", m["to"], m["status"]);
            }
        }
        Ok(())
    }
}

/// [Infobip](https://www.infobip.com/docs/api/channels/sms/send-sms-message).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Infobip {
    /// Your account's personal base URL, e.g. `https://abc123.api.infobip.com`.
    pub base_url: String,
    pub api_key: String,
    #[serde(deserialize_with = "string_or_number")]
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default = "default_max")]
    pub max_length: usize,
}

impl Infobip {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let destinations: Vec<_> = self.to.iter().map(|t| json!({ "to": t.trim_start_matches('+') })).collect();
        let body = json!({ "messages": [{ "from": self.from, "destinations": destinations, "text": text(msg, self.max_length) }] });
        let req = client
            .post(api_url(&format!("{}/sms/2/text/advanced", trim_slash(&self.base_url))))
            .header("Authorization", format!("App {}", self.api_key))
            .json(&body);
        send_ok(req).await.map(drop)
    }
}

/// [TextBelt](https://textbelt.com): a simple paid SMS API. With `key = "textbelt"` (the default)
/// it sends one free text per day, which is only good for testing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextBelt {
    /// Your API key. Default "textbelt" (one free message per day).
    #[serde(default = "textbelt_key")]
    pub key: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default = "default_max")]
    pub max_length: usize,
}

fn textbelt_key() -> String {
    "textbelt".into()
}

impl TextBelt {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        for to in &self.to {
            let form = [("phone", to.clone()), ("message", text(msg, self.max_length)), ("key", self.key.clone())];
            let reply = send_json(client.post(api_url("https://textbelt.com/text")).form(&form)).await?;
            if reply["success"] != true {
                bail!("TextBelt: {}", reply["error"].as_str().unwrap_or("message not sent"));
            }
        }
        Ok(())
    }
}

/// Serde helper: an optional value that may be written as a string or a number.
fn opt_string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    string_or_number(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::exchange;

    #[tokio::test]
    async fn twilio() {
        let toml = "type='twilio'\naccount_sid='AC1'\nauth_token='tok'\nfrom='+1555'\nto='+1666'";
        let (r, req) = exchange(toml, 201, "{}").await;
        r.unwrap();
        assert_eq!(req.line(), "POST /2010-04-01/Accounts/AC1/Messages.json HTTP/1.1");
        assert!(req.header("authorization").unwrap().starts_with("Basic "));
        assert!(req.body.contains("To=%2B1666") && req.body.contains("From=%2B1555") && req.body.contains("Body=DOWN"));
    }

    #[tokio::test]
    async fn vonage_reports_rejection() {
        let toml = "type='vonage'\napi_key='k'\napi_secret='s'\nfrom='Monitor'\nto='+1666'";
        let (r, req) = exchange(toml, 200, r#"{"messages":[{"status":"4","error-text":"Bad Credentials"}]}"#).await;
        assert!(req.body.contains("to=1666"));
        assert!(format!("{:#}", r.unwrap_err()).contains("Bad Credentials"));
    }

    #[tokio::test]
    async fn clicksend_and_textbelt() {
        let toml = "type='clicksend'\nusername='u'\napi_key='k'\nto=['+1666','+1777']";
        let (r, req) =
            exchange(toml, 200, r#"{"data":{"messages":[{"status":"SUCCESS"},{"status":"SUCCESS"}]}}"#).await;
        r.unwrap();
        assert_eq!(req.json()["messages"][1]["to"], "+1777");

        let (r, _) = exchange("type='textbelt'\nto='+1666'", 200, r#"{"success":false,"error":"Out of quota"}"#).await;
        assert!(format!("{:#}", r.unwrap_err()).contains("Out of quota"));
    }

    #[tokio::test]
    async fn sms_text_is_truncated() {
        let toml = "type='sinch'\nservice_plan_id='p'\napi_token='t'\nfrom=1555\nto='+1666'\nmax_length=20";
        let (r, req) = exchange(toml, 201, "{}").await;
        r.unwrap();
        assert_eq!(req.line(), "POST /xms/v1/p/batches HTTP/1.1");
        assert_eq!(req.json()["body"].as_str().unwrap().chars().count(), 20);
        assert_eq!(req.json()["from"], "1555");
    }
}
