//! Email providers: SMTP (any mail server) and the HTTP APIs of common email-sending services.
//!
//! Each struct is the settings of one `[[notifier]]` type and has a `send` method; they're listed in
//! [`super::NotifierKind`] and called by the dispatcher in `notify/mod.rs`. All of them share
//! `from`, `to` (one address or a list) and `format`:
//!
//! - `format = "full"` (default): subject is the message title, body is the full text with the
//!   state of every check;
//! - `format = "short"`: the body is the one-line SMS text. Use this when `to` is a carrier's
//!   email-to-SMS address (e.g. `5551234567@tmomail.net`), which turns email into a text message.

use std::time::Duration;

use anyhow::{Context, Result};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use serde::Deserialize;
use serde_json::json;

use super::{api_url, send_ok};
use crate::config::one_or_many;
use crate::message::Message;

/// Which rendering of the message goes in the email body.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Full text.
    #[default]
    Full,
    /// The one-line SMS text (for email-to-SMS gateways).
    Short,
}

/// Subject and body for `msg` in the given format.
fn render(msg: &Message, format: Format) -> (String, String) {
    match format {
        Format::Full => (msg.title.clone(), msg.body.clone()),
        Format::Short => (msg.title.clone(), msg.short.clone()),
    }
}

/// How the SMTP connection is secured.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    /// Plain connection upgraded with STARTTLS (normally port 587). The default.
    #[default]
    Starttls,
    /// TLS from the start ("SMTPS", normally port 465).
    Tls,
    /// No encryption (normally port 25; only for a relay on your own network).
    None,
}

/// SMTP: Gmail, Outlook.com, iCloud, Fastmail, Amazon SES, your ISP or your own mail server.
///
/// For Gmail use `host = "smtp.gmail.com"`, your address as `username` and an *app password*
/// (Google Account > Security > App passwords) as `password`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    /// Mail server, e.g. "smtp.gmail.com".
    pub host: String,
    /// Port. Default: 587 for starttls, 465 for tls, 25 for none.
    pub port: Option<u16>,
    /// "starttls" (default), "tls" or "none".
    #[serde(default)]
    pub security: Security,
    /// Login name, if the server needs one (almost all do).
    pub username: Option<String>,
    pub password: Option<String>,
    /// Sender, e.g. "Process Monitor <me@gmail.com>".
    pub from: String,
    /// Recipient(s).
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default)]
    pub format: Format,
}

impl Smtp {
    pub async fn send(&self, msg: &Message) -> Result<()> {
        let email = self.build(msg)?;
        type T = AsyncSmtpTransport<Tokio1Executor>;
        let mut transport = match self.security {
            Security::Starttls => T::starttls_relay(&self.host)?,
            Security::Tls => T::relay(&self.host)?,
            Security::None => T::builder_dangerous(&self.host),
        };
        let port = self.port.unwrap_or(match self.security {
            Security::Starttls => 587,
            Security::Tls => 465,
            Security::None => 25,
        });
        transport = transport.port(port).timeout(Some(Duration::from_secs(30)));
        if let Some(u) = &self.username {
            transport = transport.credentials(Credentials::new(u.clone(), self.password.clone().unwrap_or_default()));
        }
        transport.build().send(email).await.context("SMTP send failed")?;
        Ok(())
    }

    /// Builds the email itself. Errors if an address can't be parsed.
    fn build(&self, msg: &Message) -> Result<lettre::Message> {
        let (subject, body) = render(msg, self.format);
        let mut b = lettre::Message::builder()
            .from(self.from.parse().with_context(|| format!("bad `from` address `{}`", self.from))?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN);
        for to in &self.to {
            b = b.to(to.parse().with_context(|| format!("bad `to` address `{to}`"))?);
        }
        Ok(b.body(body)?)
    }
}

/// [SendGrid](https://www.twilio.com/docs/sendgrid/api-reference/mail-send/mail-send).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendGrid {
    pub api_key: String,
    /// A verified sender address.
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default)]
    pub format: Format,
}

impl SendGrid {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let (subject, body) = render(msg, self.format);
        let to: Vec<_> = self.to.iter().map(|e| json!({ "email": e })).collect();
        let req =
            client.post(api_url("https://api.sendgrid.com/v3/mail/send")).bearer_auth(&self.api_key).json(&json!({
                "personalizations": [{ "to": to }],
                "from": { "email": self.from },
                "subject": subject,
                "content": [{ "type": "text/plain", "value": body }],
            }));
        send_ok(req).await.map(drop)
    }
}

/// [Mailgun](https://documentation.mailgun.com/docs/mailgun/api-reference/).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mailgun {
    pub api_key: String,
    /// Your sending domain, e.g. "mg.example.com".
    pub domain: String,
    /// "us" (default) or "eu", matching where the domain was created.
    #[serde(default = "us")]
    pub region: String,
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default)]
    pub format: Format,
}

fn us() -> String {
    "us".into()
}

impl Mailgun {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let (subject, body) = render(msg, self.format);
        let host = if self.region.eq_ignore_ascii_case("eu") { "api.eu.mailgun.net" } else { "api.mailgun.net" };
        let url = api_url(&format!("https://{host}/v3/{}/messages", self.domain));
        let mut form = vec![("from", self.from.clone()), ("subject", subject), ("text", body)];
        form.extend(self.to.iter().map(|t| ("to", t.clone())));
        send_ok(client.post(url).basic_auth("api", Some(&self.api_key)).form(&form)).await.map(drop)
    }
}

/// [Postmark](https://postmarkapp.com/developer/api/email-api).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Postmark {
    /// Server API token.
    pub server_token: String,
    /// A confirmed sender signature.
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default)]
    pub format: Format,
}

impl Postmark {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let (subject, body) = render(msg, self.format);
        let req = client
            .post(api_url("https://api.postmarkapp.com/email"))
            .header("X-Postmark-Server-Token", &self.server_token)
            .header("Accept", "application/json")
            .json(&json!({ "From": self.from, "To": self.to.join(","), "Subject": subject, "TextBody": body }));
        send_ok(req).await.map(drop)
    }
}

/// [Resend](https://resend.com/docs/api-reference/emails/send-email).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resend {
    pub api_key: String,
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default)]
    pub format: Format,
}

impl Resend {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let (subject, body) = render(msg, self.format);
        let req = client
            .post(api_url("https://api.resend.com/emails"))
            .bearer_auth(&self.api_key)
            .json(&json!({ "from": self.from, "to": self.to, "subject": subject, "text": body }));
        send_ok(req).await.map(drop)
    }
}

/// [Brevo](https://developers.brevo.com/reference/sendtransacemail) (formerly Sendinblue).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Brevo {
    pub api_key: String,
    /// A verified sender address.
    pub from: String,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default)]
    pub format: Format,
}

impl Brevo {
    pub async fn send(&self, client: &reqwest::Client, msg: &Message) -> Result<()> {
        let (subject, body) = render(msg, self.format);
        let to: Vec<_> = self.to.iter().map(|e| json!({ "email": e })).collect();
        let req =
            client.post(api_url("https://api.brevo.com/v3/smtp/email")).header("api-key", &self.api_key).json(&json!({
                "sender": { "email": self.from },
                "to": to,
                "subject": subject,
                "textContent": body,
            }));
        send_ok(req).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::super::NotifierKind;
    use super::super::test_support::{exchange, kind, message};

    #[test]
    fn smtp_message_builds() {
        let NotifierKind::Smtp(s) = kind(
            "type='smtp'\nhost='smtp.gmail.com'\nfrom='Monitor <me@example.com>'\nto=['a@example.com','5551234567@tmomail.net']\nformat='short'",
        ) else {
            panic!()
        };
        let raw = String::from_utf8(s.build(&message()).unwrap().formatted()).unwrap();
        assert!(raw.contains("Subject: DOWN: Gallery"), "{raw}");
        assert!(raw.contains("5551234567@tmomail.net"));
        assert!(raw.contains("Others: ssh ok"));
    }

    #[test]
    fn smtp_bad_address_is_reported() {
        let NotifierKind::Smtp(s) = kind("type='smtp'\nhost='h'\nfrom='not an address'\nto='a@example.com'") else {
            panic!()
        };
        assert!(format!("{:#}", s.build(&message()).unwrap_err()).contains("from"));
    }

    #[tokio::test]
    async fn sendgrid_and_mailgun() {
        let (r, req) = exchange("type='sendgrid'\napi_key='k'\nfrom='me@x.com'\nto='you@x.com'", 202, "").await;
        r.unwrap();
        assert_eq!(req.json()["personalizations"][0]["to"][0]["email"], "you@x.com");

        let (r, req) =
            exchange("type='mailgun'\napi_key='k'\ndomain='mg.x.com'\nfrom='me@x.com'\nto='you@x.com'", 200, "{}")
                .await;
        r.unwrap();
        assert_eq!(req.line(), "POST /v3/mg.x.com/messages HTTP/1.1");
        assert!(req.body.contains("to=you%40x.com"));
    }
}
