//! process-monitor: checks websites and servers regularly and sends alerts (email, SMS, push) when
//! they stop answering and when they come back.
//!
//! This is the program's entry point. It parses the command line, sets up logging, loads the
//! config file (`config.rs`) and then either runs the monitor loop (`monitor.rs`) or one of the
//! helper commands:
//!
//! - `process-monitor` / `process-monitor run`: monitor until stopped (what the service runs);
//! - `process-monitor check`: run every check once and print the results;
//! - `process-monitor test-notify`: send a test message through every notifier (or one of them);
//! - `process-monitor validate`: check the config file and list what it contains.

mod checks;
mod config;
mod message;
mod monitor;
mod notify;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{CheckKind, EventKind};
use crate::message::Message;
use crate::notify::Dispatcher;

/// Command-line arguments.
#[derive(Parser)]
#[command(version, about = "Monitors websites and servers and alerts you by email, SMS or push when they go down.")]
struct Cli {
    /// Config file. Default: monitor.toml in the current directory, else next to the program.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// Log more detail (same as RUST_LOG=debug).
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Monitor until stopped (the default).
    Run,
    /// Run every check once, print the results and exit (status 1 if an alerting check failed).
    Check,
    /// Send a test message through every notifier, or only the one named with --notifier.
    TestNotify {
        /// Name of the notifier to test (see `validate` for the names).
        #[arg(short, long)]
        notifier: Option<String>,
    },
    /// Check the config file and list its checks and notifiers.
    Validate,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Sets up logging to stderr (systemd's journal picks it up on Linux). RUST_LOG overrides the level.
fn init_logging(verbose: bool) {
    let default = if verbose { "process_monitor=debug" } else { "process_monitor=info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    // Colours only on a terminal, so the systemd journal and log files stay plain text.
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_ansi(ansi).init();
}

/// Loads the config and runs the chosen command. Returns the process exit code.
async fn run(cli: Cli) -> Result<ExitCode> {
    let path = config::locate(cli.config.as_deref())?;
    let cfg = config::load(&path)?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            tracing::info!("using config {}", path.display());
            monitor::Monitor::new(cfg)?.run().await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Check => check_once(cfg).await,
        Command::TestNotify { notifier } => test_notify(cfg, notifier.as_deref()).await,
        Command::Validate => {
            validate(&cfg, &path);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `check`: runs each check once and prints a line per check.
async fn check_once(cfg: config::Config) -> Result<ExitCode> {
    let checkers =
        cfg.checks.into_iter().map(|c| checks::Checker::new(c, &cfg.monitor.user_agent)).collect::<Result<Vec<_>>>()?;
    let outcomes = monitor::run_all(&checkers).await;
    let mut failed = false;
    for (c, o) in checkers.iter().zip(&outcomes) {
        let mark = if o.ok { "OK  " } else { "FAIL" };
        let note = if c.config.alert { "" } else { "  (diagnostic only)" };
        println!("{mark} {} ({}): {}{note}", c.config.name, c.target(), o.detail);
        failed |= !o.ok && c.config.alert;
    }
    Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

/// `test-notify`: sends a test message and prints each notifier's result.
async fn test_notify(cfg: config::Config, only: Option<&str>) -> Result<ExitCode> {
    let name = cfg.monitor.name.clone();
    let dispatcher = Dispatcher::new(cfg.notifiers, &cfg.monitor.user_agent)?;
    let now = chrono::Local::now().format("%H:%M");
    let msg = Message {
        event: EventKind::Test,
        title: format!("Test from {name}"),
        body: format!("This is a test message from {name}. If you can read it, this notifier works."),
        short: format!("Test from {name} at {now}: this notifier works."),
        checks: Vec::new(),
        monitor: name,
    };
    let mut ok = true;
    for (n, result) in dispatcher.send_now(&msg, only).await? {
        match result {
            Ok(()) => println!("OK   {n}"),
            Err(e) => {
                ok = false;
                println!("FAIL {n}: {e:#}");
            }
        }
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

/// `validate`: prints a summary of the loaded config.
fn validate(cfg: &config::Config, path: &std::path::Path) {
    let m = &cfg.monitor;
    println!("{} is valid.\n", path.display());
    println!(
        "Monitor \"{}\": every {}, alert after {} failure(s), recover after {} success(es), reminders {}.",
        m.name,
        humantime::format_duration(m.interval),
        m.failures_before_alert,
        m.successes_before_recovery,
        match m.repeat_alert_every.filter(|d| !d.is_zero()) {
            Some(d) => format!("every {}", humantime::format_duration(d)),
            None => "off".into(),
        }
    );
    println!("\nChecks:");
    for c in &cfg.checks {
        let what = match &c.kind {
            CheckKind::Http(h) => format!("{} {}", h.method, h.url),
            CheckKind::Tcp(t) => format!("TCP {}", t.address),
        };
        let note = if c.alert { "" } else { " (diagnostic only)" };
        println!("  {}: {what}{note}", c.name);
    }
    println!("\nNotifiers:");
    if cfg.notifiers.is_empty() {
        println!("  (none: problems will only be logged)");
    }
    for n in &cfg.notifiers {
        let events: Vec<_> = n.events.iter().map(|e| e.as_str()).collect();
        println!("  {} ({}): {}", n.name, n.kind.type_name(), events.join(", "));
    }
    if let Some(h) = &cfg.heartbeat {
        println!("\nHeartbeat: every {}", humantime::format_duration(h.interval));
    }
    if let Some(p) = cfg.outage_log_path() {
        println!("\nOutage log: {}", p.display());
    }
}
