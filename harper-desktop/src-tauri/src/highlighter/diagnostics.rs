//! Opt-in reporting for an overlay that is not drawing.
//!
//! The overlay fails silently by construction. Every layer degrades to "no
//! highlights" without an error: the accessibility client can decline to give a
//! focused element, the focused app can be one the user has not enabled, the
//! geometry can resolve to nothing, and the renderer can be told there is
//! nothing new to draw. From the screen all of these look identical, and
//! several have taken days to tell apart.
//!
//! Setting `HARPER_OVERLAY_DEBUG=1` makes each layer report what it decided,
//! at most once every few seconds. Logging is at WARN because the highlighter's
//! subscriber is capped there, so INFO would go nowhere.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How often each call site may report.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Whether `HARPER_OVERLAY_DEBUG` was set, read once.
pub fn enabled() -> bool {
    static ENABLED: AtomicBool = AtomicBool::new(false);
    static READ: AtomicBool = AtomicBool::new(false);

    if !READ.swap(true, Ordering::Relaxed) {
        ENABLED.store(
            std::env::var_os("HARPER_OVERLAY_DEBUG").is_some(),
            Ordering::Relaxed,
        );
    }

    ENABLED.load(Ordering::Relaxed)
}

/// Rate limiter for one call site.
pub struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Whether this call site may report now.
    pub fn ready(&mut self) -> bool {
        if !enabled() {
            return false;
        }

        let now = Instant::now();
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < REPORT_INTERVAL)
        {
            return false;
        }

        self.last = Some(now);
        true
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}
