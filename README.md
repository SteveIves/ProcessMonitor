# Process Monitor

A small program that checks your websites and servers every minute and alerts you by **email, SMS or
push notification** when they stop answering, and again when they come back. One self-contained
executable for Linux (including Raspberry Pi) and Windows, configured with one text file.

- **HTTP checks**: status code, optional required page text, timeout. Explains Cloudflare's 52x
  errors ("origin connection timed out" and so on).
- **TCP checks**: is a port (for example SSH) open? This tells you whether the machine is alive
  even when the website isn't.
- **Diagnostic checks** (`alert = false`) never alert themselves but are listed in every alert, so
  an alert tells you *what* failed: Cloudflare, the web server, the whole machine, or the
  monitor's own internet connection.
- **No false alarms**: alerts only after N failures in a row, recovery only after M successes,
  reminders while something stays down.
- **28 notification providers**, as many at once as you like, each limited to the events you want:

  | Push and chat | SMS | Email |
  |---|---|---|
  | ntfy, Pushover, Gotify, Telegram, Discord, Slack (also Mattermost, Rocket.Chat), Microsoft Teams, Google Chat, Matrix, Pushbullet, Signal, Bark, PagerDuty, generic webhook | Twilio, Vonage, Plivo, Telnyx, Sinch, ClickSend, Infobip, TextBelt, plus free carrier email-to-SMS addresses through any email notifier | SMTP (Gmail, Outlook, iCloud, Amazon SES, any server), SendGrid, Mailgun, Postmark, Resend, Brevo |

- **Outage log**: every failure, outage and recovery with a timestamp, to compare with the
  server's own logs afterwards.
- **Heartbeat**: pings a "dead man's switch" service such as healthchecks.io, so you also hear
  about it if the monitor itself dies.

Failed alerts are retried for about 9 minutes, in case it's the monitor's own connection that's
down.

## Quick start

1. Get the program for your machine (see [Download or build](#download-or-build)).
2. Copy `monitor.example.toml` to `monitor.toml` and edit it: your sites, your server's local
   address and at least one notifier. `notifiers.example.toml` has a ready-made block for every
   provider.
3. Try it:

   ```sh
   ./process-monitor validate      # is the config right? lists checks and notifiers
   ./process-monitor check         # run every check once
   ./process-monitor test-notify   # send a test message through every notifier
   ./process-monitor test-notify --notifier email   # ... or just one
   ./process-monitor               # monitor until Ctrl+C
   ```

4. Install it as a service: [Linux / Raspberry Pi](#running-on-linux--raspberry-pi) or
   [Windows](#running-on-windows).

`process-monitor` looks for `monitor.toml` in the current folder and then next to the program; use
`--config <file>` to point elsewhere. `--verbose` logs more detail.

## Configuration

`monitor.example.toml` is a complete, commented example, and `notifiers.example.toml` documents
every notifier. Both are checked by the test suite, so they always match the program. In short:

```toml
[monitor]
name = "pi-monitor"            # shown in every message
interval = "1m"                # time between rounds of checks
failures_before_alert = 3      # failures in a row before "DOWN"
successes_before_recovery = 2  # successes in a row before "UP"
repeat_alert_every = "1h"      # reminders while down ("0s" = off)
notify_on_start = true         # "pi-monitor started" message
outage_log = "outages.log"     # relative to the config file

[[check]]
name = "Gallery"
type = "http"
url = "https://photos.example.com/"
timeout = "15s"                # default 10s
# contains = "PhotoGallery"    # text that must be on the page
# expect_status = [200]        # default: any 2xx or 3xx
# method = "HEAD"              # default GET
# headers = { Host = "photos.example.com" }
# insecure = true              # accept self-signed certificates (local network checks)
# follow_redirects = false     # default true
# alert = false                # diagnostic only

[[check]]
name = "server-ssh"
type = "tcp"
address = "192.168.1.50:22"
alert = false

[[notifier]]
type = "ntfy"
topic = "something-unguessable"

[[notifier]]
name = "text"                  # optional; used in logs and test-notify --notifier
type = "twilio"
events = ["down", "up"]        # optional; default: down, up, reminder, startup, test
account_sid = "AC..."
auth_token = "${TWILIO_TOKEN}"
from = "+15550001111"
to = "+15557654321"

[heartbeat]
url = "https://hc-ping.com/your-uuid"
interval = "5m"
```

Unknown or misspelled keys are reported as errors instead of being ignored.

### What the messages look like

Push and email get a title and the full text:

```
DOWN: Gallery

Gallery: timed out after 15s (failing since 14:01:05)

Status of all checks:
✗ Gallery (https://photos.example.com/): timed out after 15s
✗ server-web (http://192.168.1.50/): timed out after 10s
✓ server-ssh (192.168.1.50:22): port open, connected in 3 ms
✓ internet (https://www.cloudflare.com/): HTTP 200 in 85 ms

pi-monitor, Fri 6 Nov 2026 14:04:05
```

SMS gets one line, cut to 160 characters (`max_length` changes that):

```
DOWN: Gallery: timed out after 15s (failing since 14:01:05). Others: server-web FAIL, server-ssh ok, internet ok. pi-monitor 14:04
```

Several checks changing in the same round are combined into one message. The recovery message
says how long the outage lasted.

### Reading an alert

With the diagnostic checks from the example config:

| Public sites | server-web | server-ssh | internet | Meaning |
|---|---|---|---|---|
| down | ok | ok | ok | Cloudflare or tunnel problem; the server is fine |
| down | down | ok | ok | The web server or PHP is stuck, but the machine is alive |
| down | down | down | ok | The whole server is hung or off |
| down | – | – | down | The monitor's own internet connection is down |

### Free SMS without an SMS account

Most US carriers turn email sent to `<number>@<gateway>` into a text message, for example
`5551234567@tmomail.net` for T-Mobile or `5551234567@vtext.com` for Verizon. Use any email notifier
with `format = "short"` and that address as `to`. Carriers have been closing these gateways (AT&T
did in 2025), so test it with `test-notify`, and consider a push notifier as well.

### Keeping secrets out of the config

Any value can contain `${NAME}`, which is replaced by the environment variable `NAME` when the
program starts (it's an error if it isn't set). Write `$${` for a literal `${`.

- **Linux service**: put `NAME=value` lines in `/etc/process-monitor/secrets.env` (mode 600). The
  service file loads it.
- **Windows**: set machine-wide variables, which the scheduled task (running as SYSTEM) can see:
  `[Environment]::SetEnvironmentVariable("SMTP_PASSWORD", "...", "Machine")`.
- **By hand**: `export SMTP_PASSWORD=...` (Linux) or `$env:SMTP_PASSWORD = "..."` (PowerShell).

## Running on Linux / Raspberry Pi

Find out which build your machine needs with `uname -m`: `aarch64` → `process-monitor-linux-arm64`;
`armv7l` → `process-monitor-linux-armv7`; `x86_64` → `process-monitor-linux-x86_64`. The Linux builds are
statically linked and need nothing else installed.

```sh
sudo install -m 755 process-monitor-linux-arm64 /usr/local/bin/process-monitor
sudo mkdir -p /etc/process-monitor
sudo install -m 644 monitor.toml /etc/process-monitor/
sudo install -m 600 /dev/null /etc/process-monitor/secrets.env   # then add NAME=value lines
sudo install -m 644 contrib/process-monitor.service /etc/systemd/system/
sudo systemctl enable --now process-monitor
```

In `/etc/process-monitor/monitor.toml`, set `outage_log = "/var/lib/process-monitor/outages.log"`. The
service runs as an unprivileged throwaway user that can only write to `/var/lib/process-monitor`.

```sh
systemctl status process-monitor           # is it running?
journalctl -u process-monitor -f           # live log
sudo cat /var/lib/process-monitor/outages.log   # outage history
sudo systemctl restart process-monitor     # after editing the config
```

Test the installed config the same way the service will run it:

```sh
sudo bash -c 'set -a; . /etc/process-monitor/secrets.env; process-monitor --config /etc/process-monitor/monitor.toml test-notify'
```

## Running on Windows

1. Put `process-monitor.exe` (`process-monitor-windows-x86_64.exe`, renamed) and `monitor.toml` in a
   folder, for example `C:\process-monitor`.
2. In PowerShell, from that folder, run `.\process-monitor.exe check` and `.\process-monitor.exe test-notify`.
3. From an **administrator** PowerShell window:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install-windows.ps1 -Folder C:\process-monitor
   ```

   This creates a scheduled task, *process-monitor*, that starts at boot (whether or not anyone is
   logged in), runs as SYSTEM, and restarts it if it stops. Remove it with
   `.\install-windows.ps1 -Uninstall`.

There is no console window, so use the outage log (`outage_log` in `monitor.toml`, relative to the
config's folder) and `Get-ScheduledTaskInfo process-monitor` to see what it's doing. After editing
the config: `Stop-ScheduledTask process-monitor; Start-ScheduledTask process-monitor`.

## Download or build

**Releases**: download the file for your machine from the
[Releases page](https://github.com/SteveIves/ProcessMonitor/releases).

To publish a release, run the **Release** workflow (Actions > Release > Run workflow, choosing
patch, minor or major). It runs the tests, increases the version number (the first run uses the
version in `Cargo.toml` as it is), updates `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md`, commits
and tags `v<version>`, builds every platform and publishes the release with the changelog section
as its notes. Record changes under `## Unreleased` at the top of `CHANGELOG.md` as you make them;
the release turns that heading into the version number and date.

**Build it yourself** with Rust (https://rustup.rs):

```sh
cargo build --release            # for this machine: target/release/process-monitor
cargo test                       # run the tests
```

**Cross-compile from x86-64 Linux** (Debian/Ubuntu). The linkers are already set in
`.cargo/config.toml`:

```sh
sudo apt install gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf gcc-mingw-w64-x86-64
rustup target add aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf \
                  x86_64-unknown-linux-musl x86_64-pc-windows-gnu
cargo build --release --target aarch64-unknown-linux-musl       # 64-bit Raspberry Pi OS
cargo build --release --target armv7-unknown-linux-musleabihf   # 32-bit Raspberry Pi OS
cargo build --release --target x86_64-unknown-linux-musl        # any x86-64 Linux
cargo build --release --target x86_64-pc-windows-gnu            # Windows
```

Building directly on the Pi also works (`cargo build --release`), though it takes several
minutes.

## How it works

Each round, every check runs at the same time. A failure starts a streak. After
`failures_before_alert` failures the check is **DOWN**, and the outage's start time is when the
streak began. A check that is already failing when the monitor starts goes down the same way; one
that is fine starts **UP** silently. A down check needs `successes_before_recovery` successes in a
row to be **UP** again. Alerts go out in the background, so a slow provider never delays the
checks, and the heartbeat is pinged only after a round completes, so it stops if the monitor ever
stalls.

| File | Purpose |
|---|---|
| `src/main.rs` | Command line, logging, the `check` / `test-notify` / `validate` commands |
| `src/config.rs` | Config file format, `${ENV}` expansion, validation |
| `src/checks.rs` | Running one HTTP or TCP check and describing the result |
| `src/monitor.rs` | The loop: up/down decisions, outage log, building messages, heartbeat |
| `src/message.rs` | The message passed to notifiers, text helpers |
| `src/notify/mod.rs` | Provider list, dispatcher with retries, shared HTTP helpers |
| `src/notify/push.rs`, `sms.rs`, `email.rs` | The providers |
| `contrib/` | systemd service file, Windows install script |
