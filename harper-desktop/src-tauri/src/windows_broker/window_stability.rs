//! Withholding highlights while a window is being dragged or resized.
//!
//! Highlight rectangles are captured at one instant and drawn at another. While
//! a window is in motion those two instants disagree, and the underlines slide
//! along behind the text they belong to. macOS solves this in
//! `mac_broker/window_stability.rs`; upstream considered it worth a dedicated
//! fix (#3419, "hide highlights while windows move"), so Windows matching that
//! behaviour is parity rather than embellishment.
//!
//! Two deliberate differences from the macOS implementation:
//!
//! 1. **Keyed by window, not process.** macOS asks CoreGraphics for the
//!    frontmost window belonging to a pid; on Windows the foreground window is
//!    already the frontmost one, and its `HWND` identifies it exactly. A
//!    process with several windows is then handled correctly for free, and
//!    stale state cannot outlive the window it describes — so this needs none
//!    of the explicit invalidation the macOS broker threads through its early
//!    returns.
//! 2. **Exact comparison, no tolerance.** macOS allows half a pixel because
//!    CoreGraphics reports bounds as `f64` that jitter. `GetWindowRect` returns
//!    integers, which do not.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};

/// How long a window must hold still before highlights return.
///
/// 150 ms, matching `WINDOW_MOVEMENT_SETTLE_DURATION` on macOS. Long enough
/// that the pause between two drag gestures does not flash highlights back on,
/// short enough not to feel like a delay after releasing the mouse.
pub const SETTLE_DURATION: Duration = Duration::from_millis(150);

/// The last observed position of the foreground window.
#[derive(Default)]
pub struct WindowMovement {
    last: Option<Sample>,
}

struct Sample {
    /// `HWND` as a plain integer: this is an identity to compare, never a
    /// handle to use, and keeping it opaque means it cannot be dereferenced
    /// after the window is gone.
    window: isize,
    frame: RECT,
    last_changed_at: Instant,
}

impl Sample {
    /// A sample that counts as already settled.
    ///
    /// Backdating rather than storing "settled" as a flag keeps the elapsed
    /// comparison the single rule that decides the question. A newly focused
    /// window has not been seen moving, so it should highlight immediately.
    fn settled(window: isize, frame: RECT, now: Instant) -> Self {
        Self {
            window,
            frame,
            last_changed_at: now.checked_sub(SETTLE_DURATION).unwrap_or(now),
        }
    }
}

impl WindowMovement {
    /// Whether the foreground window is moving, or stopped too recently to
    /// trust.
    pub fn is_moving(&mut self) -> bool {
        let Some((window, frame)) = foreground_window_frame() else {
            // Treated as movement, matching macOS: with no readable geometry
            // the safe answer is to withhold highlights rather than draw them
            // at a position that may no longer exist.
            self.last = None;
            return true;
        };

        self.observe(window, frame, Instant::now())
    }

    /// The decision itself, separated from reading the screen so it can be
    /// tested without a window to drag.
    fn observe(&mut self, window: isize, frame: RECT, now: Instant) -> bool {
        let Some(sample) = &mut self.last else {
            self.last = Some(Sample::settled(window, frame, now));
            return false;
        };

        // A different window is not the same window having moved. Switching
        // focus should highlight at once, not wait out a settle period.
        if sample.window != window {
            *sample = Sample::settled(window, frame, now);
            return false;
        }

        if frame_changed(sample.frame, frame) {
            sample.frame = frame;
            sample.last_changed_at = now;
            return true;
        }

        now.duration_since(sample.last_changed_at) < SETTLE_DURATION
    }
}

fn frame_changed(previous: RECT, current: RECT) -> bool {
    previous.left != current.left
        || previous.top != current.top
        || previous.right != current.right
        || previous.bottom != current.bottom
}

/// Identity and screen rectangle of the foreground window.
fn foreground_window_frame() -> Option<(isize, RECT)> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        let mut frame = RECT::default();
        GetWindowRect(hwnd, &mut frame).ok()?;

        Some((hwnd.0 as isize, frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: isize = 0x1234;
    const OTHER_WINDOW: isize = 0x5678;

    fn rect(left: i32, top: i32) -> RECT {
        RECT {
            left,
            top,
            right: left + 800,
            bottom: top + 600,
        }
    }

    /// Covers the half the logic tests cannot: that the Win32 call is wired up
    /// and returns something usable.
    ///
    /// Whether a *drag* is detected end to end stays a manual check — a
    /// background process cannot take the foreground on Windows, so no
    /// automated test can put a window under the cursor and move it. The smoke
    /// protocol carries it instead.
    #[test]
    fn the_foreground_frame_is_readable() {
        // CI agents and local runs both always have some foreground window.
        if let Some((window, frame)) = foreground_window_frame() {
            assert_ne!(window, 0, "a real window handle is never null here");
            assert!(
                frame.right >= frame.left && frame.bottom >= frame.top,
                "GetWindowRect returned an inverted rectangle: {frame:?}"
            );
        }
    }

    #[test]
    fn a_newly_seen_window_highlights_immediately() {
        let mut movement = WindowMovement::default();

        assert!(
            !movement.observe(WINDOW, rect(0, 0), Instant::now()),
            "focusing a window is not movement; waiting would delay every switch"
        );
    }

    #[test]
    fn a_moved_window_is_moving() {
        let mut movement = WindowMovement::default();
        let now = Instant::now();

        movement.observe(WINDOW, rect(0, 0), now);

        assert!(
            movement.observe(WINDOW, rect(40, 0), now + Duration::from_millis(16)),
            "the frame changed, which is the definition of moving"
        );
    }

    #[test]
    fn a_resized_window_is_moving() {
        let mut movement = WindowMovement::default();
        let now = Instant::now();

        movement.observe(WINDOW, rect(0, 0), now);
        let mut resized = rect(0, 0);
        resized.right += 120;

        assert!(
            movement.observe(WINDOW, resized, now + Duration::from_millis(16)),
            "a drag on the window edge moves text just as a drag on the title bar does"
        );
    }

    #[test]
    fn highlights_stay_hidden_until_the_window_settles() {
        let mut movement = WindowMovement::default();
        let start = Instant::now();

        movement.observe(WINDOW, rect(0, 0), start);
        movement.observe(WINDOW, rect(40, 0), start);

        assert!(
            movement.observe(WINDOW, rect(40, 0), start + Duration::from_millis(100)),
            "still within the settle window, so highlights stay hidden"
        );
        assert!(
            !movement.observe(WINDOW, rect(40, 0), start + SETTLE_DURATION),
            "the window has held still long enough to draw on"
        );
    }

    #[test]
    fn switching_windows_does_not_count_as_movement() {
        let mut movement = WindowMovement::default();
        let now = Instant::now();

        movement.observe(WINDOW, rect(0, 0), now);
        movement.observe(WINDOW, rect(40, 0), now);

        // Mid-settle for the first window, but this is a different one.
        assert!(
            !movement.observe(
                OTHER_WINDOW,
                rect(900, 300),
                now + Duration::from_millis(20)
            ),
            "a window at a different position is not the previous one having moved"
        );
    }

    #[test]
    fn a_still_window_keeps_highlighting_indefinitely() {
        let mut movement = WindowMovement::default();
        let start = Instant::now();

        movement.observe(WINDOW, rect(0, 0), start);

        assert!(
            !movement.observe(WINDOW, rect(0, 0), start + Duration::from_secs(3600)),
            "a window that never moves must never stop highlighting"
        );
    }
}
