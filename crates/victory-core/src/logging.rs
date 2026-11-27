//! One tracing setup for every binary.
//!
//! Filter with `VICTORY_LOG` (falls back to `RUST_LOG`), e.g.
//! `VICTORY_LOG=victory_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.
//!
//! `VICTORY_LOG_FILE=<path>`: every line is added to that file too, without
//! colours (`victory play` names one for each start).

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

pub fn init(default_level: &str) {
    let filter = std::env::var("VICTORY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level));
    // (Opened again right after it was made: a virus scanner may hold a new
    // file for a moment, so a refusal is tried again a few times.)
    let open = |path: &std::ffi::OsString| {
        let mut tries = 0;
        loop {
            match std::fs::OpenOptions::new().create(true).append(true).open(path) {
                Ok(file) => return Some(file),
                Err(_) if tries < 20 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    eprintln!("VICTORY_LOG_FILE: no log file ({e}); this run logs to this window only");
                    return None;
                }
            }
        }
    };
    let file = std::env::var_os("VICTORY_LOG_FILE")
        .filter(|path| !path.is_empty())
        .and_then(|path| open(&path))
        .map(|file| {
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
        });
    let _ = tracing_subscriber::registry()
        .with(CAP)
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::io::stderr),
        )
        .with(file)
        .try_init();
}
