mod highlighter_process;
mod highlighter_worker;

use crate::config::Config;
use highlighter_worker::HighlighterWorker;
use std::{
    io,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// How long a restarted highlighter must survive before the backoff is
/// forgiven. Shorter than this and a process that dies a few seconds after
/// every start would be respawned forever at the shortest delay.
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

/// Backoff state for restarting a highlighter that exited unexpectedly.
#[derive(Default)]
struct RestartState {
    attempts: u32,
    next_attempt_at: Option<Instant>,
    /// When this service last started the highlighter itself.
    last_start: Option<Instant>,
}

/// Wraps around a [`HighlighterWorker`] and turns it into a service that is easier to manage.
/// We can start and stop it and simply provide a shared `Config` object that we can update whenever.
/// The service will sync the changes to the underlying highlighter process.
pub struct HighlighterService {
    config: Arc<Mutex<Config>>,
    worker: StdMutex<Option<HighlighterWorker>>,
    /// Whether the highlighter is meant to be running, so an unexpected exit
    /// can be told apart from a deliberate stop.
    should_run: AtomicBool,
    restart: StdMutex<RestartState>,
}

impl HighlighterService {
    /// Create a new highlighter process service, without starting it.
    /// The provided config pointer will be automatically synced to the new process when changes are
    /// made.
    pub fn new(config: Arc<Mutex<Config>>) -> Self {
        Self {
            config,
            worker: StdMutex::new(None),
            should_run: AtomicBool::new(false),
            restart: StdMutex::new(RestartState::default()),
        }
    }

    /// Restarts the highlighter if it exited without being asked to.
    ///
    /// The highlighter is a separate process, so a panic in it — a display
    /// reconfiguration invalidating a monitor handle, a driver fault — takes
    /// highlighting away silently, leaving only a stopped tray icon that the
    /// user has to notice. Restart attempts back off so a highlighter that
    /// cannot start does not spin.
    pub fn ensure_running(&self) {
        if !self.should_run.load(Ordering::Relaxed) {
            return;
        }

        if self.is_running() {
            // Forgive the backoff only once the process has stayed up. Clearing
            // it on a single healthy observation would let a highlighter that
            // crashes a few seconds after every start respawn indefinitely at
            // the shortest delay, since each restart is briefly observed alive.
            let mut restart = self
                .restart
                .lock()
                .expect("highlighter restart lock poisoned");
            if restart
                .last_start
                .is_some_and(|at| at.elapsed() >= HEALTHY_UPTIME)
            {
                *restart = RestartState::default();
            }
            return;
        }

        {
            let mut restart = self
                .restart
                .lock()
                .expect("highlighter restart lock poisoned");
            let now = Instant::now();
            if restart.next_attempt_at.is_some_and(|at| now < at) {
                return;
            }
            restart.attempts = restart.attempts.saturating_add(1);
            let backoff = Duration::from_secs(1 << restart.attempts.min(6));
            restart.next_attempt_at = Some(now + backoff);
        }

        tracing::warn!("highlighter is not running; restarting it");
        match self.start() {
            Ok(_) => {
                self.restart
                    .lock()
                    .expect("highlighter restart lock poisoned")
                    .last_start = Some(Instant::now());
            }
            Err(error) => tracing::error!("could not restart the highlighter: {error}"),
        }
    }

    /// Starts the highlighter worker if it is not already running.
    pub fn start(&self) -> io::Result<bool> {
        self.should_run.store(true, Ordering::Relaxed);
        self.reap_finished_worker();

        let mut worker = self
            .worker
            .lock()
            .expect("highlighter service lock poisoned");
        if worker.is_some() {
            return Ok(true);
        }

        *worker = Some(HighlighterWorker::spawn(self.config.clone())?);

        Ok(true)
    }

    /// Stops the highlighter worker if one is running.
    pub fn stop(&self) -> bool {
        // Deliberate stop: do not let the supervisor undo it.
        self.should_run.store(false, Ordering::Relaxed);

        let worker = self
            .worker
            .lock()
            .expect("highlighter service lock poisoned")
            .take();

        if let Some(mut worker) = worker {
            worker.stop();
        }

        false
    }

    /// Starts or stops the worker based on the current state.
    ///
    /// The tray menu uses this as its single action for the highlighter service. Returns whether the
    /// service is running after the toggle completes.
    pub fn toggle(&self) -> io::Result<bool> {
        if self.is_running() {
            Ok(self.stop())
        } else {
            self.start()
        }
    }

    /// Reports whether a live worker is currently owned by the service.
    pub fn is_running(&self) -> bool {
        self.reap_finished_worker();

        self.worker
            .lock()
            .expect("highlighter service lock poisoned")
            .is_some()
    }

    /// Joins and removes a worker whose thread has already exited.
    fn reap_finished_worker(&self) {
        let worker = {
            let mut worker = self
                .worker
                .lock()
                .expect("highlighter service lock poisoned");
            if worker.as_ref().is_some_and(HighlighterWorker::is_finished) {
                worker.take()
            } else {
                None
            }
        };

        if let Some(mut worker) = worker {
            worker.stop();
        }
    }
}

impl Drop for HighlighterService {
    /// Stops the worker when the Tauri-managed service is dropped.
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .expect("highlighter service lock poisoned");

        if let Some(mut worker) = worker.take() {
            worker.stop();
        }
    }
}
