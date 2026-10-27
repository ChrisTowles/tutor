//! Logging for `tutor` and `tutor-station`: one `tracing` subscriber that
//! writes to stderr and to an event log on disk, one JSON object per line, at
//! `$XDG_DATA_HOME/tutor/telemetry/events-<date>.jsonl` (UTC days, 14 kept).
//!
//! The log is there to answer questions later ("when did Logan's PC lag?",
//! "how long did we tutor this week?") without a live repro, so every user
//! action and every connection change emits an event; see the README.

mod event_log;
mod layer;

use std::path::PathBuf;

use serde_json::{Map, Value};
use tracing_subscriber::{EnvFilter, prelude::*};

pub use event_log::EventLog;
pub use layer::EventLogLayer;

const RETAIN_DAYS: usize = 14;

/// `TUTOR_TELEMETRY=0` turns the disk log off; stderr keeps working.
const DISABLE_ENV: &str = "TUTOR_TELEMETRY";

/// Our crates at debug whatever `RUST_LOG` says (a quiet terminal must not
/// mean a useless log); dependencies only when they warn.
const DISK_FILTER: &str = "warn,tutor=debug,tutor_station=debug,tutor_telemetry=debug";

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
        .join("tutor/telemetry")
}

/// Install the global subscriber for `service` ("tutor" or "tutor-station"),
/// first thing in `main`. stderr shows `info` unless `RUST_LOG` says otherwise.
pub fn init(service: &str) {
    let disk = std::env::var(DISABLE_ENV)
        .map_or(true, |v| !matches!(v.trim(), "0" | "false"))
        .then(|| EventLogLayer::new(EventLog::new(dir(), RETAIN_DAYS), resource(service)).with_filter(EnvFilter::new(DISK_FILTER)));
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    let _ = tracing_subscriber::registry().with(stderr).with(disk).try_init();
    install_panic_hook();
}

/// Who wrote a record, in OpenTelemetry naming.
fn resource(service: &str) -> Map<String, Value> {
    let host = std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
    Map::from_iter([
        ("service.name".to_string(), Value::from(service)),
        ("service.version".to_string(), Value::from(env!("CARGO_PKG_VERSION"))),
        ("process.pid".to_string(), Value::from(std::process::id())),
        ("host.name".to_string(), Value::from(host)),
    ])
}

/// A panic is an `app.crash` record before the usual report.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        tracing::error!(target: "tutor_telemetry", %message, %location, %thread, "app.crash");
        default(info);
    }));
}
