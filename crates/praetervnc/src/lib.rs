//! PraeterVNC: a latency-first VNC (RFB) server for Windows.
#[macro_export]
macro_rules! log {
    ($($t:tt)*) => {{
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        eprintln!("[{}.{:03}] {}", t.as_secs() % 86400, t.subsec_millis(), format!($($t)*));
    }};
}

pub mod auth;
pub mod capture;
pub mod clipboard;
pub mod desktop;
pub mod enc;
pub mod fb;
pub mod gui;
pub mod input;
pub mod install;
pub mod ipc;
pub mod net;
pub mod pipe;
pub mod pixfmt;
pub mod probe;
pub mod rfb;
pub mod scroll;
pub mod server;
pub mod service;
pub mod session;
pub mod settings;
pub mod plan;
pub mod watchdog;
pub mod simd;
pub mod window;
pub mod writer;
pub mod zstream;

/// Tuning and process-level options; user settings live in `settings`.
pub struct Config {
    /// Listen address override (CLI).
    pub bind: Option<String>,
    /// Service helper: never listen without a password.
    pub service: bool,
    pub name: String,
    pub verbose: bool,
    pub max_inflight: u32,
    pub alr_ms: u64,
    pub scroll: bool,
    pub budget_ms: f64,
    pub motion: bool,
    pub pipeline: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: None, service: false, name: "PraeterVNC".into(), verbose: false,
            max_inflight: 32, alr_ms: 150, scroll: true, budget_ms: 25.0, motion: true, pipeline: true,
        }
    }
}
