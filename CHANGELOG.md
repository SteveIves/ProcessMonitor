# Changelog

Newest first. Add changes under "## Unreleased"; the Release workflow replaces that heading with the
version number and date when it publishes a release.

## Unreleased

First version.

- HTTP checks (status codes, required page text, custom headers, self-signed certificates,
  Cloudflare 52x explanations) and TCP port checks, run concurrently every `interval`.
- Down after `failures_before_alert` failures in a row, up after `successes_before_recovery`
  successes in a row, reminders every `repeat_alert_every`; diagnostic checks (`alert = false`)
  listed in every alert.
- 28 notifiers: ntfy, Pushover, Gotify, Telegram, Discord, Slack, Teams, Google Chat, Matrix,
  Pushbullet, Signal, Bark, PagerDuty, webhook; Twilio, Vonage, Plivo, Telnyx, Sinch, ClickSend,
  Infobip, TextBelt; SMTP, SendGrid, Mailgun, Postmark, Resend, Brevo. Per-notifier event filters,
  background delivery with retries for about 9 minutes.
- Outage log, heartbeat (dead man's switch) pings, optional start-up message.
- `${ENV}` expansion in the config; strict checking of unknown keys.
- Commands: `run` (default), `check`, `test-notify [--notifier]`, `validate`.
- systemd service file, Windows scheduled-task install script, CI workflow, and a Release workflow
  that increases the version number automatically and publishes static Linux (x86-64, arm64,
  armv7), Windows and macOS binaries.
