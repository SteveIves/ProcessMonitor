//! Configuration file loading for process-monitor.
//!
//! The monitor is driven entirely by a TOML file (normally `monitor.toml`). This module defines the
//! shape of that file, finds it on disk, expands `${ENV_VAR}` references in its string values (so
//! secrets can live in the environment instead of the file), and validates it. `main.rs` calls
//! [`load`] once at start-up; the resulting [`Config`] is handed to the monitor loop and notifiers.
//!
//! Checks and notifiers share a few common keys (`name`, `alert`, `events`, ...) and then have
//! type-specific keys. To keep strict "unknown key" errors for the type-specific part, each entry is
//! read as a raw table, the common keys are removed, and the rest is deserialized into a tagged enum
//! whose variants reject unknown fields. A typo such as `tokn = "..."` is therefore reported instead
//! of being silently ignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::notify::NotifierKind;

/// The whole configuration file.
#[derive(Debug)]
pub struct Config {
    /// Global monitor settings (`[monitor]`).
    pub monitor: MonitorSettings,
    /// Everything to check (`[[check]]`), in file order.
    pub checks: Vec<CheckConfig>,
    /// Where alerts go (`[[notifier]]`), in file order.
    pub notifiers: Vec<NotifierConfig>,
    /// Optional dead-man's-switch ping (`[heartbeat]`).
    pub heartbeat: Option<HeartbeatConfig>,
    /// Directory containing the config file; relative paths in the config are resolved against it.
    pub base_dir: PathBuf,
}

/// `[monitor]`: timing and behaviour shared by all checks.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MonitorSettings {
    /// Name of this monitor, shown in every message (useful when several monitors run).
    pub name: String,
    /// Time between rounds of checks.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Consecutive failed rounds before a check is declared down and an alert is sent.
    pub failures_before_alert: u32,
    /// Consecutive successful rounds before a down check is declared back up.
    pub successes_before_recovery: u32,
    /// While a check stays down, send a reminder this often. Absent or "0s" disables reminders.
    #[serde(with = "humantime_serde")]
    pub repeat_alert_every: Option<Duration>,
    /// Send a "monitor started" message when the program starts (to notifiers that want it).
    pub notify_on_start: bool,
    /// File that every up/down transition is appended to, for later comparison with server logs.
    pub outage_log: Option<PathBuf>,
    /// User-Agent header sent with HTTP checks.
    pub user_agent: String,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            name: "process-monitor".into(),
            interval: Duration::from_secs(60),
            failures_before_alert: 3,
            successes_before_recovery: 2,
            repeat_alert_every: Some(Duration::from_secs(3600)),
            notify_on_start: false,
            outage_log: None,
            user_agent: concat!("process-monitor/", env!("CARGO_PKG_VERSION")).into(),
        }
    }
}

/// One `[[check]]` entry.
#[derive(Debug, Clone)]
pub struct CheckConfig {
    /// Unique name used in messages and the outage log.
    pub name: String,
    /// Whether this check going down or up sends alerts. Checks with `alert = false` are still run
    /// and listed in every alert, so they work as diagnostics ("is the server's SSH port open?").
    pub alert: bool,
    /// How long to wait for an answer before counting the attempt as failed.
    pub timeout: Duration,
    /// What to check.
    pub kind: CheckKind,
}

/// The type-specific part of a check, selected by its `type` key.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckKind {
    /// Fetch a URL and check the status code (and optionally the page text).
    Http(HttpCheck),
    /// Open a TCP connection to `host:port` (e.g. SSH on port 22) and close it again.
    Tcp(TcpCheck),
}

/// Settings for `type = "http"` checks.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpCheck {
    /// URL to fetch.
    pub url: String,
    /// HTTP method; GET by default.
    #[serde(default = "default_method")]
    pub method: String,
    /// Status codes that count as success. Default: any 2xx or 3xx.
    #[serde(default)]
    pub expect_status: Vec<u16>,
    /// Text that must appear in the response body. Catches "server answers but the app is broken".
    pub contains: Option<String>,
    /// Accept invalid or self-signed TLS certificates (for checks of a server on the local network).
    #[serde(default)]
    pub insecure: bool,
    /// Follow redirects (up to 10). Default true.
    #[serde(default = "default_true")]
    pub follow_redirects: bool,
    /// Extra request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// Settings for `type = "tcp"` checks.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpCheck {
    /// `host:port` to connect to, e.g. `192.168.1.50:22`.
    pub address: String,
}

/// One `[[notifier]]` entry.
#[derive(Debug, Clone)]
pub struct NotifierConfig {
    /// Name used in logs and with `test-notify --notifier`. Defaults to the type, numbered if needed.
    pub name: String,
    /// Which events this notifier receives. Default: all of them.
    pub events: Vec<EventKind>,
    /// Provider and its settings.
    pub kind: NotifierKind,
}

/// The kinds of message the monitor sends; notifiers can subscribe to a subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A check has just been declared down.
    Down,
    /// A check that was down is answering again.
    Up,
    /// A check is still down (sent every `repeat_alert_every`).
    Reminder,
    /// The monitor has started (only if `notify_on_start = true`).
    Startup,
    /// Sent by the `test-notify` command.
    Test,
}

impl EventKind {
    /// Every event kind; the default subscription of a notifier.
    pub const ALL: [EventKind; 5] = [Self::Down, Self::Up, Self::Reminder, Self::Startup, Self::Test];

    /// Lower-case name, as used in the config file and in webhook payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::Reminder => "reminder",
            Self::Startup => "startup",
            Self::Test => "test",
        }
    }
}

/// `[heartbeat]`: a URL fetched regularly while the monitor is healthy, for services such as
/// healthchecks.io that alert you when the pings stop (i.e. when the monitor itself has died).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatConfig {
    /// URL to fetch (GET).
    pub url: String,
    /// Minimum time between pings. Default: 5 minutes.
    #[serde(with = "humantime_serde", default = "default_heartbeat_interval")]
    pub interval: Duration,
}

fn default_method() -> String {
    "GET".into()
}
fn default_true() -> bool {
    true
}
fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(300)
}

/// Works out which config file to use: the `--config` argument if given, otherwise `monitor.toml`
/// in the current directory, otherwise `monitor.toml` next to the executable (which is what a
/// Windows scheduled task or service needs, as those start in `C:\Windows\System32`).
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let here = PathBuf::from("monitor.toml");
    if here.exists() {
        return Ok(here);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside = dir.join("monitor.toml");
        if beside.exists() {
            return Ok(beside);
        }
    }
    bail!("no monitor.toml in the current directory or next to the program; use --config <file>")
}

/// Reads, expands and validates the config file at `path`.
///
/// Errors: the file can't be read, isn't valid TOML, refers to an unset environment variable,
/// has unknown or missing keys, or fails validation (see [`Config::validate`]).
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let base_dir =
        path.canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."));
    parse(&text, base_dir).with_context(|| format!("in {}", path.display()))
}

/// Parses config text (separate from [`load`] so tests can use it without files).
pub fn parse(text: &str, base_dir: PathBuf) -> Result<Config> {
    let mut root: toml::Table = toml::from_str(text)?;
    expand_env_table(&mut root)?;

    let monitor = match root.remove("monitor") {
        Some(v) => MonitorSettings::deserialize(v).context("in [monitor]")?,
        None => MonitorSettings::default(),
    };
    let heartbeat = match root.remove("heartbeat") {
        Some(v) => Some(HeartbeatConfig::deserialize(v).context("in [heartbeat]")?),
        None => None,
    };
    let checks = take_array(&mut root, "check")?
        .into_iter()
        .enumerate()
        .map(|(i, t)| parse_check(t).with_context(|| format!("in [[check]] number {}", i + 1)))
        .collect::<Result<Vec<_>>>()?;
    let mut notifiers = take_array(&mut root, "notifier")?
        .into_iter()
        .enumerate()
        .map(|(i, t)| parse_notifier(t).with_context(|| format!("in [[notifier]] number {}", i + 1)))
        .collect::<Result<Vec<_>>>()?;
    if let Some(key) = root.keys().next() {
        bail!("unknown top-level key `{key}` (expected monitor, check, notifier or heartbeat)");
    }
    name_notifiers(&mut notifiers);

    let config = Config { monitor, checks, notifiers, heartbeat, base_dir };
    config.validate()?;
    Ok(config)
}

impl Config {
    /// Checks rules serde can't express: at least one check, unique names, sane numbers.
    fn validate(&self) -> Result<()> {
        if self.checks.is_empty() {
            bail!("no [[check]] entries: nothing to monitor");
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.checks {
            if !seen.insert(c.name.as_str()) {
                bail!("two checks are named `{}`; names must be unique", c.name);
            }
            if let CheckKind::Http(h) = &c.kind {
                reqwest::Url::parse(&h.url).with_context(|| format!("check `{}`: bad url", c.name))?;
                reqwest::Method::from_bytes(h.method.as_bytes())
                    .map_err(|_| anyhow!("check `{}`: bad method `{}`", c.name, h.method))?;
            }
        }
        if !self.checks.iter().any(|c| c.alert) {
            bail!("every check has alert = false, so no alert would ever be sent");
        }
        let mut seen = std::collections::HashSet::new();
        for n in &self.notifiers {
            if !seen.insert(n.name.as_str()) {
                bail!("two notifiers are named `{}`; names must be unique", n.name);
            }
        }
        let m = &self.monitor;
        if m.interval < Duration::from_secs(1) {
            bail!("[monitor] interval must be at least 1s");
        }
        if m.failures_before_alert == 0 || m.successes_before_recovery == 0 {
            bail!("[monitor] failures_before_alert and successes_before_recovery must be at least 1");
        }
        Ok(())
    }

    /// The outage log path, resolved relative to the config file's directory.
    pub fn outage_log_path(&self) -> Option<PathBuf> {
        self.monitor.outage_log.as_ref().map(|p| self.base_dir.join(p))
    }
}

/// Removes `key` from `root` and returns it as a list of tables (`[[key]]` entries).
fn take_array(root: &mut toml::Table, key: &str) -> Result<Vec<toml::Table>> {
    match root.remove(key) {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                toml::Value::Table(t) => Ok(t),
                _ => Err(anyhow!("`{key}` entries must be tables; write them as [[{key}]]")),
            })
            .collect(),
        Some(_) => bail!("`{key}` must be written as [[{key}]] sections"),
    }
}

/// Builds one check from its table: common keys first, the rest by `type`.
fn parse_check(mut t: toml::Table) -> Result<CheckConfig> {
    let name = take_string(&mut t, "name")?.ok_or_else(|| anyhow!("missing `name`"))?;
    let alert = match t.remove("alert") {
        None => true,
        Some(toml::Value::Boolean(b)) => b,
        Some(_) => bail!("`alert` must be true or false"),
    };
    let timeout = match take_string(&mut t, "timeout")? {
        None => Duration::from_secs(10),
        Some(s) => humantime::parse_duration(&s).with_context(|| format!("bad timeout `{s}`"))?,
    };
    let kind = CheckKind::deserialize(toml::Value::Table(t)).with_context(|| format!("check `{name}`"))?;
    Ok(CheckConfig { name, alert, timeout, kind })
}

/// Builds one notifier from its table: common keys first, the rest by `type`.
fn parse_notifier(mut t: toml::Table) -> Result<NotifierConfig> {
    let name = take_string(&mut t, "name")?.unwrap_or_default();
    let events = match t.remove("events") {
        None => EventKind::ALL.to_vec(),
        Some(v) => {
            Vec::<EventKind>::deserialize(v).context("`events` must be a list of: down, up, reminder, startup, test")?
        }
    };
    let kind = NotifierKind::deserialize(toml::Value::Table(t))?;
    Ok(NotifierConfig { name, events, kind })
}

/// Gives unnamed notifiers a name: their type, with a number added when the type repeats.
fn name_notifiers(notifiers: &mut [NotifierConfig]) {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for n in notifiers.iter() {
        if n.name.is_empty() {
            *counts.entry(n.kind.type_name()).or_default() += 1;
        }
    }
    let mut used: BTreeMap<&'static str, usize> = BTreeMap::new();
    for n in notifiers.iter_mut() {
        if n.name.is_empty() {
            let ty = n.kind.type_name();
            let i = used.entry(ty).or_default();
            *i += 1;
            n.name = if counts[ty] > 1 { format!("{ty}-{i}") } else { ty.to_string() };
        }
    }
}

/// Removes `key` from `t`, requiring it to be a string if present.
fn take_string(t: &mut toml::Table, key: &str) -> Result<Option<String>> {
    match t.remove(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s)),
        Some(_) => bail!("`{key}` must be a string"),
    }
}

/// Expands `${NAME}` in every string of the table, recursively.
fn expand_env_table(t: &mut toml::Table) -> Result<()> {
    for (_, v) in t.iter_mut() {
        expand_env_value(v)?;
    }
    Ok(())
}

fn expand_env_value(v: &mut toml::Value) -> Result<()> {
    match v {
        toml::Value::String(s) => *s = expand_env(s)?,
        toml::Value::Array(a) => a.iter_mut().try_for_each(expand_env_value)?,
        toml::Value::Table(t) => expand_env_table(t)?,
        _ => {}
    }
    Ok(())
}

/// Replaces each `${NAME}` in `s` with the environment variable NAME. `$${` produces a literal `${`.
///
/// Errors if a referenced variable isn't set or a `${` is never closed.
pub fn expand_env(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        if let Some(escaped) = after.strip_prefix("$${") {
            out.push_str("${");
            rest = escaped;
        } else if let Some(inner) = after.strip_prefix("${") {
            let end = inner.find('}').ok_or_else(|| anyhow!("unclosed `${{` in `{s}`"))?;
            let name = &inner[..end];
            let value = std::env::var(name)
                .map_err(|_| anyhow!("environment variable `{name}` is not set (used in the config)"))?;
            out.push_str(&value);
            rest = &inner[end + 1..];
        } else {
            out.push('$');
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Serde helper: accepts a string or an integer and returns a string. Used for values such as
/// Telegram chat ids and phone numbers that people naturally write either way in TOML.
pub fn string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Sn {
        S(String),
        N(i64),
    }
    Ok(match Sn::deserialize(d)? {
        Sn::S(s) => s,
        Sn::N(n) => n.to_string(),
    })
}

/// Serde helper: accepts a single string or a list of strings. Used for `to = ...` recipients.
pub fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Om {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Om::deserialize(d)? {
        Om::One(s) => vec![s],
        Om::Many(v) => v,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [[check]]
        name = "site"
        type = "http"
        url = "https://example.com/"
    "#;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = parse(MINIMAL, ".".into()).unwrap();
        assert_eq!(c.monitor.interval, Duration::from_secs(60));
        assert_eq!(c.checks[0].timeout, Duration::from_secs(10));
        assert!(c.checks[0].alert);
        assert!(c.notifiers.is_empty());
    }

    #[test]
    fn unknown_check_key_is_rejected() {
        let text = format!("{MINIMAL}\ncontians = \"x\"\n");
        let err = format!("{:#}", parse(&text, ".".into()).unwrap_err());
        assert!(err.contains("contians"), "{err}");
    }

    #[test]
    fn unknown_notifier_key_is_rejected() {
        let text = format!("{MINIMAL}\n[[notifier]]\ntype = \"ntfy\"\ntopic = \"t\"\ntokn = \"x\"\n");
        let err = format!("{:#}", parse(&text, ".".into()).unwrap_err());
        assert!(err.contains("tokn"), "{err}");
    }

    #[test]
    fn notifiers_are_named_after_type() {
        let text = format!(
            "{MINIMAL}\n[[notifier]]\ntype = \"ntfy\"\ntopic = \"a\"\n[[notifier]]\ntype = \"ntfy\"\ntopic = \"b\"\n\
             [[notifier]]\ntype = \"slack\"\nwebhook_url = \"https://x\"\nevents = [\"down\"]\n"
        );
        let c = parse(&text, ".".into()).unwrap();
        let names: Vec<_> = c.notifiers.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["ntfy-1", "ntfy-2", "slack"]);
        assert_eq!(c.notifiers[2].events, [EventKind::Down]);
    }

    #[test]
    fn env_expansion() {
        // SAFETY: tests in this module don't read this variable concurrently.
        unsafe { std::env::set_var("PROCESS_MONITOR_TEST_VAR", "secret") };
        assert_eq!(expand_env("a${PROCESS_MONITOR_TEST_VAR}b").unwrap(), "asecretb");
        assert_eq!(expand_env("cost $5 $${X}").unwrap(), "cost $5 ${X}");
        assert!(expand_env("${PROCESS_MONITOR_UNSET_VAR_XYZ}").is_err());
        assert!(expand_env("${OOPS").is_err());
    }

    #[test]
    fn example_config_is_valid() {
        // SAFETY: only this test reads this variable.
        unsafe { std::env::set_var("SMTP_PASSWORD", "x") };
        let c = parse(include_str!("../monitor.example.toml"), ".".into()).unwrap();
        assert_eq!(c.checks.len(), 5);
        assert!(!c.notifiers.is_empty());
    }

    #[test]
    fn every_provider_is_in_notifiers_example() {
        let text = format!("{MINIMAL}\n{}", include_str!("../notifiers.example.toml"));
        let c = parse(&text, ".".into()).unwrap();
        let mut types: Vec<_> = c.notifiers.iter().map(|n| n.kind.type_name()).collect();
        types.sort();
        types.dedup();
        // Keep in step with NotifierKind: every provider must be documented by an example.
        assert_eq!(types.len(), 28, "{types:?}");
    }

    #[test]
    fn duplicate_check_names_rejected() {
        let text = format!("{MINIMAL}{MINIMAL}");
        assert!(parse(&text, ".".into()).is_err());
    }
}
