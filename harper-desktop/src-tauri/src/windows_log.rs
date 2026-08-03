//! File logging, because on Windows stderr has nowhere to go.
//!
//! `main.rs` sets `windows_subsystem = "windows"` for release builds, so a user
//! who launches Harper normally has no console attached and every diagnostic is
//! discarded. Redirecting stderr at launch works and is what the smoke-test
//! protocol does, but that only helps someone who already suspected a problem
//! and restarted the app to catch it — which is precisely the information you
//! do not have after an unattended failure.
//!
//! macOS has no equivalent gap: stderr from a bundled app is captured by the
//! unified log, so a mac user can produce a diagnostic on request. This module
//! gives Windows the same ability rather than a better one.
//!
//! Two decisions here are deliberate and worth not undoing:
//!
//! 1. **Writes are synchronous.** `tracing_appender::non_blocking` is the
//!    usual choice and is wrong for this build: the release profile sets
//!    `panic = "abort"`, so a panicking process does not run destructors and a
//!    buffered writer loses its tail — exactly the lines describing the crash.
//!    Volume is a couple of lines per five seconds at most, so blocking writes
//!    cost nothing worth having.
//! 2. **Panics are routed through `tracing`.** Rust's default panic handler
//!    writes to stderr, which is the void this module exists to work around. A
//!    panic that leaves no trace is how the invalidated-monitor-handle bug
//!    stayed hidden across four separate attempts to fix it.

use std::panic;
use std::path::{Path, PathBuf};

use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// Which process is logging. They are separate processes writing concurrently,
/// so they get separate files rather than interleaving into one.
#[derive(Clone, Copy)]
pub enum Role {
    Main,
    Highlighter,
}

impl Role {
    fn filename_prefix(self) -> &'static str {
        match self {
            Self::Main => "harper",
            Self::Highlighter => "harper-highlighter",
        }
    }
}

/// Directory holding the rotated log files.
///
/// Local app data rather than roaming: logs are machine-specific and should not
/// follow a roaming profile around. Sibling of the config directory, which
/// `config::Config` puts under `dirs::config_dir()`.
pub fn directory() -> Option<PathBuf> {
    dirs::data_local_dir().map(|path| path.join("harper-desktop").join("logs"))
}

/// A synchronous, daily-rotating appender for this process.
///
/// `None` when the directory cannot be resolved or created, in which case the
/// caller keeps stderr alone — logging must never be the reason Harper fails
/// to start.
pub fn appender(role: Role) -> Option<RollingFileAppender> {
    appender_in(&directory()?, role)
}

/// The work behind [`appender`], with the directory injected.
///
/// Separate so tests can write somewhere disposable. Building an appender
/// creates a file as a side effect, so a test calling [`appender`] directly
/// would deposit its output in the user's real log directory and leave it
/// there among the diagnostics someone is trying to read.
fn appender_in(directory: &Path, role: Role) -> Option<RollingFileAppender> {
    if let Err(error) = std::fs::create_dir_all(directory) {
        eprintln!("could not create the log directory {directory:?}: {error}");
        return None;
    }

    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(role.filename_prefix())
        .filename_suffix("log")
        // Enough history to cover a failure noticed the following Monday, which
        // is how the dormancy bugs were actually reported.
        .max_log_files(5)
        .build(directory)
        .inspect_err(|error| eprintln!("could not open a log file in {directory:?}: {error}"))
        .ok()
}

/// Routes panics through `tracing` so they reach the log file.
///
/// Chains to the previous hook rather than replacing it, so the standard
/// stderr message still appears for anyone running with a console attached.
pub fn install_panic_hook() {
    let previous = panic::take_hook();

    panic::set_hook(Box::new(move |info| {
        // Forced rather than `capture()`, which honours `RUST_BACKTRACE` and so
        // would be empty for every user who has not set it — that is, everyone
        // whose crash we would want to read about.
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {info}\n{backtrace}");

        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_directory_sits_beside_the_config_directory() {
        let logs = directory().expect("a local data directory on Windows");

        assert_eq!(logs.file_name().unwrap(), "logs");
        assert_eq!(
            logs.parent().unwrap().file_name().unwrap(),
            "harper-desktop"
        );
    }

    #[test]
    fn roles_write_to_separate_files() {
        assert_ne!(
            Role::Main.filename_prefix(),
            Role::Highlighter.filename_prefix(),
            "the two processes run concurrently and would interleave"
        );
    }

    #[test]
    fn appender_writes_synchronously() {
        use std::io::Write as _;

        // Never the real log directory: building an appender creates a file,
        // and a test has no business leaving one among a user's diagnostics.
        let directory = std::env::temp_dir().join(format!(
            "harper-log-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));

        let mut appender =
            appender_in(&directory, Role::Main).expect("an appender under the temp directory");
        writeln!(appender, "a line").expect("a synchronous appender needs no runtime to flush");

        let written = std::fs::read_dir(&directory)
            .expect("the directory is created as a side effect of building the appender")
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("harper."))
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .unwrap_or_default();

        // The point of the synchronous writer: readable without dropping or
        // flushing anything, because an aborting process does neither.
        assert!(
            written.contains("a line"),
            "expected the line on disk while the appender is still alive, found {written:?}"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The load-bearing claim of `install_panic_hook`: a panic leaves a record.
    ///
    /// Rust's default hook writes to stderr, which on an installed Windows
    /// build goes nowhere, so without this the only evidence of a crash is the
    /// absence of the process.
    #[test]
    fn panics_reach_tracing() {
        use std::io;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .expect("test buffer poisoned")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for Buffer {
            type Writer = Self;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .finish();

        install_panic_hook();

        tracing::subscriber::with_default(subscriber, || {
            // Unwinds here because tests are a debug build; in release this
            // same path aborts, which is why the write must already be on disk.
            let _ = panic::catch_unwind(|| panic!("deliberate panic from the test suite"));
        });

        let logged =
            String::from_utf8_lossy(&buffer.0.lock().expect("test buffer poisoned")).into_owned();

        assert!(
            logged.contains("deliberate panic from the test suite"),
            "the panic hook logged nothing usable, found {logged:?}"
        );
    }
}
