//! The monitoring loop: runs every check each round, decides when a check is down or back up,
//! writes the outage log, sends alerts and pings the heartbeat.
//!
//! `main.rs` creates a [`Monitor`] from the loaded config and calls [`Monitor::run`], which loops
//! until Ctrl+C (or SIGTERM on Linux). Each round:
//!
//! 1. all checks run concurrently ([`crate::checks::Checker::run`]);
//! 2. each result is fed to that check's [`CheckState`], which applies the
//!    `failures_before_alert` / `successes_before_recovery` / `repeat_alert_every` rules and may
//!    report a [`Transition`];
//! 3. transitions are appended to the outage log, and those of checks with `alert = true` are
//!    grouped into one [`Message`] per kind and handed to the [`Dispatcher`];
//! 4. if a heartbeat is configured and due, it is pinged.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use tracing::{info, warn};

use crate::checks::{Checker, Outcome};
use crate::config::{Config, EventKind, HeartbeatConfig, MonitorSettings};
use crate::message::{Message, human_duration};
use crate::notify::Dispatcher;

/// Up/down state of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not enough results yet (just started and failing, but not yet `failures_before_alert` times).
    Unknown,
    /// Answering normally.
    Up,
    /// Declared down (an alert has been sent for alerting checks).
    Down,
}

/// A change worth telling someone about.
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    /// The check has failed `failures_before_alert` times in a row; it started failing at `since`.
    Down { since: DateTime<Local> },
    /// The check is answering again after being down for `downtime`.
    Up { downtime: chrono::TimeDelta },
    /// The check is still down (it went down at `since`); sent every `repeat_alert_every`.
    Reminder { since: DateTime<Local> },
}

/// Counters and timestamps that decide when a check changes state.
#[derive(Debug, Clone)]
pub struct CheckState {
    /// Current status.
    pub status: Status,
    /// Consecutive failures in the current streak.
    fails: u32,
    /// Consecutive successes in the current streak.
    oks: u32,
    /// When the current failure streak began (before the check was declared down).
    first_fail: Option<DateTime<Local>>,
    /// When the check went down (the start of the failure streak that took it down).
    down_since: Option<DateTime<Local>>,
    /// When the last down/reminder alert was sent, for spacing reminders.
    last_alert: Option<DateTime<Local>>,
    /// Detail text of the most recent result.
    pub last: Option<Outcome>,
}

impl Default for CheckState {
    fn default() -> Self {
        Self {
            status: Status::Unknown,
            fails: 0,
            oks: 0,
            first_fail: None,
            down_since: None,
            last_alert: None,
            last: None,
        }
    }
}

impl CheckState {
    /// Records one result taken at `now` and returns the resulting transition, if any.
    ///
    /// A check that is fine when the monitor starts goes straight to `Up` without an alert. A
    /// check that is failing when the monitor starts goes `Down` (with an alert) once it has failed
    /// `failures_before_alert` times, just like one that was up before.
    pub fn record(&mut self, outcome: Outcome, now: DateTime<Local>, s: &MonitorSettings) -> Option<Transition> {
        let ok = outcome.ok;
        self.last = Some(outcome);
        if ok {
            self.fails = 0;
            self.oks += 1;
            match self.status {
                Status::Unknown | Status::Up => {
                    self.status = Status::Up;
                    self.first_fail = None;
                    None
                }
                Status::Down if self.oks >= s.successes_before_recovery => {
                    self.status = Status::Up;
                    self.first_fail = None;
                    self.last_alert = None;
                    let since = self.down_since.take().unwrap_or(now);
                    Some(Transition::Up { downtime: now - since })
                }
                Status::Down => None,
            }
        } else {
            self.oks = 0;
            self.fails += 1;
            match self.status {
                Status::Down => {
                    let since = self.down_since.unwrap_or(now);
                    let every = s.repeat_alert_every.filter(|d| !d.is_zero())?;
                    let last = self.last_alert.unwrap_or(since);
                    if now - last >= chrono::TimeDelta::from_std(every).unwrap_or(chrono::TimeDelta::MAX) {
                        self.last_alert = Some(now);
                        Some(Transition::Reminder { since })
                    } else {
                        None
                    }
                }
                Status::Unknown | Status::Up => {
                    let first = *self.first_fail.get_or_insert(now);
                    if self.fails >= s.failures_before_alert {
                        self.status = Status::Down;
                        self.down_since = Some(first);
                        self.last_alert = Some(now);
                        Some(Transition::Down { since: first })
                    } else {
                        None
                    }
                }
            }
        }
    }

    /// Whether the latest result was a failure (even if the check isn't declared down yet).
    pub fn failing(&self) -> bool {
        self.last.as_ref().is_some_and(|o| !o.ok)
    }

    /// Whether the check has failed exactly once so far in the current streak; used to log the
    /// start of a problem even if it clears up before an alert is sent.
    fn just_started_failing(&self) -> bool {
        self.fails == 1 && self.status != Status::Down
    }
}

/// The running monitor.
pub struct Monitor {
    settings: MonitorSettings,
    checkers: Vec<Checker>,
    states: Vec<CheckState>,
    dispatcher: Arc<Dispatcher>,
    outage_log: Option<PathBuf>,
    heartbeat: Option<Heartbeat>,
}

/// Heartbeat configuration plus when it was last pinged.
struct Heartbeat {
    config: HeartbeatConfig,
    client: reqwest::Client,
    last: Option<tokio::time::Instant>,
}

impl Monitor {
    /// Builds the monitor from a validated config.
    ///
    /// Errors if a check's or notifier's HTTP client can't be created (e.g. a bad header).
    pub fn new(config: Config) -> Result<Self> {
        let outage_log = config.outage_log_path();
        let Config { monitor: settings, checks, notifiers, heartbeat, .. } = config;
        let checkers = checks.into_iter().map(|c| Checker::new(c, &settings.user_agent)).collect::<Result<Vec<_>>>()?;
        let states = vec![CheckState::default(); checkers.len()];
        let heartbeat = heartbeat
            .map(|config| -> Result<Heartbeat> {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(15))
                    .user_agent(settings.user_agent.as_str())
                    .build()?;
                Ok(Heartbeat { config, client, last: None })
            })
            .transpose()?;
        Ok(Self {
            dispatcher: Arc::new(Dispatcher::new(notifiers, &settings.user_agent)?),
            settings,
            checkers,
            states,
            outage_log,
            heartbeat,
        })
    }

    /// Runs rounds of checks every `interval` until Ctrl+C or SIGTERM.
    pub async fn run(mut self) -> Result<()> {
        info!(
            "monitoring {} check(s) every {}, alerting {} notifier(s)",
            self.checkers.len(),
            humantime::format_duration(self.settings.interval),
            self.dispatcher.len()
        );
        let mut ticker = tokio::time::interval(self.settings.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut first = true;
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown_signal() => {
                    info!("stopping");
                    // Give alerts that are still being sent a moment to finish.
                    self.dispatcher.wait_idle(Duration::from_secs(10)).await;
                    return Ok(());
                }
            }
            self.round().await;
            if first && self.settings.notify_on_start {
                self.dispatcher.send(self.startup_message());
            }
            first = false;
        }
    }

    /// Runs every check once and acts on the results.
    async fn round(&mut self) {
        let outcomes = run_all(&self.checkers).await;
        let now = Local::now();
        let mut down = Vec::new();
        let mut up = Vec::new();
        let mut reminders = Vec::new();
        for (i, outcome) in outcomes.into_iter().enumerate() {
            let check = &self.checkers[i].config;
            let ok = outcome.ok;
            let detail = outcome.detail.clone();
            let transition = self.states[i].record(outcome, now, &self.settings);
            if !ok && self.states[i].just_started_failing() {
                info!("{} failing: {detail}", check.name);
                self.log_line(now, "FAILING", &check.name, &detail);
            }
            let Some(t) = transition else { continue };
            match &t {
                Transition::Down { .. } => {
                    warn!("{} is DOWN: {detail}", check.name);
                    self.log_line(now, "DOWN", &check.name, &detail);
                }
                Transition::Up { downtime } => {
                    info!("{} is UP again after {}", check.name, human_duration(*downtime));
                    self.log_line(now, "UP", &check.name, &format!("{detail}; was down {}", human_duration(*downtime)));
                }
                Transition::Reminder { .. } => warn!("{} still down: {detail}", check.name),
            }
            if check.alert {
                match t {
                    Transition::Down { since } => down.push((i, since)),
                    Transition::Up { downtime } => up.push((i, downtime)),
                    Transition::Reminder { since } => reminders.push((i, since)),
                }
            }
        }
        if !down.is_empty() {
            let lines = down
                .iter()
                .map(|&(i, since)| self.line(i, &format!("failing since {}", fmt_time(since, now))))
                .collect();
            self.dispatcher.send(self.compose(EventKind::Down, "DOWN", &down, lines, now));
        }
        if !up.is_empty() {
            let lines = up.iter().map(|&(i, d)| self.line(i, &format!("was down {}", human_duration(d)))).collect();
            self.dispatcher.send(self.compose(EventKind::Up, "UP", &up, lines, now));
        }
        if !reminders.is_empty() {
            let lines = reminders
                .iter()
                .map(|&(i, since)| self.line(i, &format!("down for {}", human_duration(now - since))))
                .collect();
            self.dispatcher.send(self.compose(EventKind::Reminder, "STILL DOWN", &reminders, lines, now));
        }
        self.heartbeat().await;
    }

    /// "Name: detail (extra)" for one check, using its latest result.
    fn line(&self, i: usize, extra: &str) -> String {
        let detail = self.states[i].last.as_ref().map(|o| o.detail.as_str()).unwrap_or("");
        format!("{}: {detail} ({extra})", self.checkers[i].config.name)
    }

    /// Builds the message for one kind of event affecting the checks `items` (indices first).
    fn compose<T>(
        &self,
        event: EventKind,
        word: &str,
        items: &[(usize, T)],
        lines: Vec<String>,
        now: DateTime<Local>,
    ) -> Message {
        let names: Vec<String> = items.iter().map(|(i, _)| self.checkers[*i].config.name.clone()).collect();
        let title = format!("{word}: {}", names.join(", "));
        let body = format!(
            "{}\n\n{}\n\n{}, {}",
            lines.join("\n"),
            self.status_table(),
            self.settings.name,
            now.format("%a %-d %b %Y %H:%M:%S")
        );
        // SMS: the event lines, then a compact state of the other checks (which tells you, for
        // example, whether the server's SSH port still answers while the website doesn't).
        let others: Vec<String> = (0..self.checkers.len())
            .filter(|i| !items.iter().any(|(j, _)| j == i))
            .filter_map(|i| {
                let state = match self.states[i].last.as_ref()?.ok {
                    true => "ok",
                    false => "FAIL",
                };
                Some(format!("{} {state}", self.checkers[i].config.name))
            })
            .collect();
        let mut short = format!("{word}: {}", lines.join("; "));
        if !others.is_empty() {
            short.push_str(&format!(". Others: {}", others.join(", ")));
        }
        short.push_str(&format!(". {} {}", self.settings.name, now.format("%H:%M")));
        Message { event, title, body, short, checks: names, monitor: self.settings.name.clone() }
    }

    /// "Status of all checks" block: one line per check with a tick or cross and its latest result.
    fn status_table(&self) -> String {
        let mut out = String::from("Status of all checks:");
        for (c, s) in self.checkers.iter().zip(&self.states) {
            let (mark, detail) = match &s.last {
                Some(o) if o.ok => ("✓", o.detail.as_str()),
                Some(o) => ("✗", o.detail.as_str()),
                None => ("?", "not checked yet"),
            };
            out.push_str(&format!("\n{mark} {} ({}): {detail}", c.config.name, c.target()));
        }
        out
    }

    /// The optional "monitor started" message, sent after the first round so it can include results.
    fn startup_message(&self) -> Message {
        let failing: Vec<&str> = self
            .checkers
            .iter()
            .zip(&self.states)
            .filter(|(_, s)| s.failing())
            .map(|(c, _)| c.config.name.as_str())
            .collect();
        let summary =
            if failing.is_empty() { "all checks OK".to_string() } else { format!("failing: {}", failing.join(", ")) };
        Message {
            event: EventKind::Startup,
            title: format!("{} started", self.settings.name),
            body: format!("Monitoring started, {summary}.\n\n{}", self.status_table()),
            short: format!("{} started, {summary}", self.settings.name),
            checks: Vec::new(),
            monitor: self.settings.name.clone(),
        }
    }

    /// Appends one line to the outage log, if configured. Failures to write are logged, not fatal.
    fn log_line(&self, now: DateTime<Local>, what: &str, name: &str, detail: &str) {
        let Some(path) = &self.outage_log else { return };
        let line = format!("{} {what:<7} {name}: {detail}\n", now.format("%Y-%m-%d %H:%M:%S %z"));
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = result {
            warn!("can't write outage log {}: {e}", path.display());
        }
    }

    /// Pings the heartbeat URL if one is configured and `interval` has passed since the last ping.
    /// Called at the end of each round, so pings stop if the loop ever stalls.
    async fn heartbeat(&mut self) {
        let Some(hb) = &mut self.heartbeat else { return };
        if hb.last.is_some_and(|t| t.elapsed() < hb.config.interval) {
            return;
        }
        hb.last = Some(tokio::time::Instant::now());
        match hb.client.get(&hb.config.url).send().await.and_then(|r| r.error_for_status()) {
            Ok(_) => tracing::debug!("heartbeat sent"),
            Err(e) => warn!("heartbeat ping failed: {e}"),
        }
    }
}

/// Runs all checks concurrently and returns their outcomes in the same order.
pub async fn run_all(checkers: &[Checker]) -> Vec<Outcome> {
    futures::future::join_all(checkers.iter().map(Checker::run)).await
}

/// "14:03:12" for today, otherwise "Thu 5 Nov 14:03".
fn fmt_time(t: DateTime<Local>, now: DateTime<Local>) -> String {
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M:%S").to_string()
    } else {
        t.format("%a %-d %b %H:%M").to_string()
    }
}

/// Resolves when the process is asked to stop: Ctrl+C everywhere, plus SIGTERM on Unix (what
/// systemd sends on `systemctl stop`).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> MonitorSettings {
        MonitorSettings {
            failures_before_alert: 3,
            successes_before_recovery: 2,
            repeat_alert_every: Some(Duration::from_secs(3600)),
            ..Default::default()
        }
    }

    fn ok() -> Outcome {
        Outcome { ok: true, detail: "ok".into() }
    }
    fn fail() -> Outcome {
        Outcome { ok: false, detail: "timed out".into() }
    }

    #[test]
    fn starts_up_silently() {
        let mut s = CheckState::default();
        assert_eq!(s.record(ok(), Local::now(), &settings()), None);
        assert_eq!(s.status, Status::Up);
    }

    #[test]
    fn goes_down_after_n_failures_and_recovers_after_m_successes() {
        let st = settings();
        let t0 = Local::now();
        let at = |m: i64| t0 + chrono::TimeDelta::minutes(m);
        let mut s = CheckState::default();
        s.record(ok(), at(0), &st);
        assert_eq!(s.record(fail(), at(1), &st), None);
        assert_eq!(s.record(fail(), at(2), &st), None);
        assert_eq!(s.record(fail(), at(3), &st), Some(Transition::Down { since: at(1) }));
        assert_eq!(s.record(fail(), at(4), &st), None);
        // One success isn't enough to recover; a failure in between keeps it down without a new alert.
        assert_eq!(s.record(ok(), at(5), &st), None);
        assert_eq!(s.record(fail(), at(6), &st), None);
        assert_eq!(s.record(ok(), at(7), &st), None);
        assert_eq!(s.record(ok(), at(8), &st), Some(Transition::Up { downtime: chrono::TimeDelta::minutes(7) }));
        assert_eq!(s.status, Status::Up);
    }

    #[test]
    fn brief_blip_does_not_alert() {
        let st = settings();
        let t0 = Local::now();
        let mut s = CheckState::default();
        s.record(ok(), t0, &st);
        assert_eq!(s.record(fail(), t0, &st), None);
        assert_eq!(s.record(fail(), t0, &st), None);
        assert_eq!(s.record(ok(), t0, &st), None);
        assert_eq!(s.record(fail(), t0, &st), None);
        assert_eq!(s.status, Status::Up);
    }

    #[test]
    fn reminders_are_spaced() {
        let st = settings();
        let t0 = Local::now();
        let at = |m: i64| t0 + chrono::TimeDelta::minutes(m);
        let mut s = CheckState::default();
        for m in 0..3 {
            s.record(fail(), at(m), &st);
        }
        assert_eq!(s.status, Status::Down);
        assert_eq!(s.record(fail(), at(30), &st), None);
        assert_eq!(s.record(fail(), at(62), &st), Some(Transition::Reminder { since: at(0) }));
        assert_eq!(s.record(fail(), at(100), &st), None);
        assert_eq!(s.record(fail(), at(122), &st), Some(Transition::Reminder { since: at(0) }));
    }

    #[test]
    fn reminders_can_be_disabled() {
        let st = MonitorSettings { repeat_alert_every: Some(Duration::ZERO), ..settings() };
        let t0 = Local::now();
        let mut s = CheckState::default();
        for m in 0..3 {
            s.record(fail(), t0 + chrono::TimeDelta::minutes(m), &st);
        }
        assert_eq!(s.record(fail(), t0 + chrono::TimeDelta::days(1), &st), None);
    }
}
