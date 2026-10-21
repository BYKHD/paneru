//! Observation-only probe for Emaki #49 (Finder lag). Writes NDJSON lines to
//! `$PANERU_PROBE_LOG` from a background thread; does nothing when unset.
//! Not for merging: it must never change engine behavior.

use std::io::Write;
use std::sync::LazyLock;
use std::sync::mpsc::{Sender, channel};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static SINK: LazyLock<Option<Sender<String>>> = LazyLock::new(|| {
    let path = std::env::var("PANERU_PROBE_LOG").ok()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let (tx, rx) = channel::<String>();
    std::thread::spawn(move || {
        for line in rx {
            _ = writeln!(file, "{line}");
        }
    });
    Some(tx)
});

/// Wall-clock milliseconds, comparable with the external sampler.
pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

pub fn enabled() -> bool {
    SINK.is_some()
}

/// `fields` is a JSON object body without braces, e.g. `"pid":1`.
pub fn emit(kind: &str, start: Instant, t0_ms: f64, fields: &str) {
    if let Some(tx) = SINK.as_ref() {
        let us = start.elapsed().as_micros();
        _ = tx.send(format!(
            "{{\"t\":{t0_ms:.3},\"k\":\"{kind}\",\"us\":{us},{fields}}}"
        ));
    }
}
