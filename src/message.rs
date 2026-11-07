//! The alert message passed to every notifier, and the text formatting shared by all of them.
//!
//! The monitor loop (`monitor.rs`) builds one [`Message`] per kind of event per round (for example
//! one "DOWN" message naming every check that went down in that round) and hands it to the
//! notifiers in `notify/`. Each message carries three renderings so every provider can use the one
//! that suits it: a `title`, a full multi-line `body` for email/push/chat, and a `short` one-line
//! text for SMS.

use crate::config::EventKind;

/// One notification.
#[derive(Debug, Clone)]
pub struct Message {
    /// What kind of event this is; notifiers filter on it.
    pub event: EventKind,
    /// Headline, e.g. "DOWN: Gallery, Blog".
    pub title: String,
    /// Full text: what happened, then the state of every check.
    pub body: String,
    /// One-line text for SMS, without emoji (emoji force SMS into a 70-character encoding).
    pub short: String,
    /// Names of the checks the event is about (used e.g. as PagerDuty de-duplication keys).
    pub checks: Vec<String>,
    /// The monitor's name from `[monitor] name`.
    pub monitor: String,
}

impl Message {
    /// `title` followed by `body`, for providers that only take a single text field.
    pub fn title_and_body(&self) -> String {
        format!("{}\n\n{}", self.title, self.body)
    }

    /// Whether this message should be treated as urgent (high priority, loud sound).
    pub fn is_urgent(&self) -> bool {
        matches!(self.event, EventKind::Down | EventKind::Reminder)
    }
}

/// Shortens `s` to at most `max` characters, ending with "..." if anything was cut.
/// Works on characters, not bytes, so it never splits a multi-byte character.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}

/// Formats a duration as e.g. "1h 4m" or "45s" (no sub-second parts, at most two units).
pub fn human_duration(d: chrono::TimeDelta) -> String {
    let secs = d.num_seconds().max(0);
    let (days, hours, mins, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, m) => {
            if s > 0 {
                format!("{m}m {s}s")
            } else {
                format!("{m}m")
            }
        }
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_counts_characters() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("ééééé", 4), "é...");
    }

    #[test]
    fn durations() {
        let d = chrono::TimeDelta::seconds;
        assert_eq!(human_duration(d(42)), "42s");
        assert_eq!(human_duration(d(125)), "2m 5s");
        assert_eq!(human_duration(d(3600 + 240 + 5)), "1h 4m");
        assert_eq!(human_duration(d(2 * 86400 + 3 * 3600)), "2d 3h");
    }
}
