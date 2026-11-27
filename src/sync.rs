//! The background synchronization worker and the state the interface reads.
//!
//! One long-lived worker thread runs the passes; the interface hands it work
//! and reads progress through nonblocking channels and atomics, so storage or
//! network delays can never block input.
use std::{
    cell::{Cell, RefCell},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    store::Store,
    todoist::{AuxiliaryRefresh, TodoistClient, permanent_rejection, retry_delay},
};

#[derive(Debug, Clone)]
pub enum SyncEvent {
    Started,
    Finished {
        pending: usize,
    },
    Failed {
        kind: SyncFailureKind,
        message: String,
    },
    AuxiliaryWarning {
        message: String,
    },
    FilterRefreshed {
        filter_id: String,
    },
    DataRefreshed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFailureKind {
    Offline,
    Problem,
}

pub struct SyncManager {
    client: Option<TodoistClient>,
    store: Store,
    running: Arc<AtomicBool>,
    tx: Sender<SyncEvent>,
    rx: Receiver<SyncEvent>,
    // The UI owns its retry snapshot. Workers publish updates instead of
    // making input/timer handling wait for a worker-held mutex.
    retry: Cell<RetryState>,
    retry_attempts: Arc<AtomicU32>,
    retry_tx: Sender<RetryState>,
    retry_rx: Receiver<RetryState>,
    auxiliary_due: Arc<Mutex<Instant>>,
    filter_refresh_requested: Arc<AtomicBool>,
    preferred_filter: RefCell<Option<String>>,
    preference_tx: Sender<Option<String>>,
    preferences: Arc<Mutex<FilterPreference>>,
    // One long-lived worker rather than a thread per pass. An exiting thread
    // closes its database connection, and SQLite's unix VFS deadlocks when a
    // close overlaps another thread's open, so steady-state synchronization
    // must stop creating and retiring threads. See `Store::connect`.
    passes: RefCell<Option<Sender<()>>>,
    #[cfg(test)]
    workers_started: Cell<u32>,
}

#[derive(Clone, Copy)]
struct RetryState {
    attempts: u32,
    deadline: Instant,
}

struct FilterPreference {
    current: Option<String>,
    updates: Receiver<Option<String>>,
}

impl FilterPreference {
    fn latest(&mut self) -> Option<String> {
        for update in self.updates.try_iter() {
            self.current = update;
        }
        self.current.clone()
    }
}

struct RunningGuard {
    running: Arc<AtomicBool>,
    finished: bool,
}

impl RunningGuard {
    fn finish(&mut self) {
        self.running.store(false, Ordering::Release);
        self.finished = true;
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.running.store(false, Ordering::Release);
        }
    }
}

impl SyncManager {
    pub fn new(store: Store, token: Option<String>) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (retry_tx, retry_rx) = mpsc::channel();
        let (preference_tx, preference_rx) = mpsc::channel();
        let retry_after = store
            .meta("retry_not_before")?
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
            .map(|date| {
                (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                    .to_std()
                    .unwrap_or(Duration::ZERO)
            })
            .unwrap_or_default();
        Ok(Self {
            client: token.map(TodoistClient::new).transpose()?,
            store,
            running: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            retry: Cell::new(RetryState {
                attempts: u32::from(!retry_after.is_zero()),
                deadline: Instant::now()
                    .checked_add(retry_after)
                    .unwrap_or_else(Instant::now),
            }),
            retry_attempts: Arc::new(AtomicU32::new(u32::from(!retry_after.is_zero()))),
            retry_tx,
            retry_rx,
            auxiliary_due: Arc::new(Mutex::new(Instant::now())),
            filter_refresh_requested: Arc::new(AtomicBool::new(false)),
            preferred_filter: RefCell::new(None),
            preference_tx,
            preferences: Arc::new(Mutex::new(FilterPreference {
                current: None,
                updates: preference_rx,
            })),
            passes: RefCell::new(None),
            #[cfg(test)]
            workers_started: Cell::new(0),
        })
    }

    pub fn has_token(&self) -> bool {
        self.client.is_some()
    }
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn request_filter_refresh(&self) {
        self.filter_refresh_requested.store(true, Ordering::Release);
    }

    pub fn filter_refresh_pending(&self) -> bool {
        self.filter_refresh_requested.load(Ordering::Acquire)
    }

    pub fn prefer_filter(&self, id: Option<&str>) {
        let mut preferred = self.preferred_filter.borrow_mut();
        if preferred.as_deref() != id {
            *preferred = id.map(str::to_owned);
            let _ = self.preference_tx.send(preferred.clone());
        }
    }

    pub fn start(&self) -> bool {
        let Some(client) = self.client.clone() else {
            return false;
        };
        if !self.retry_after().is_zero() {
            return false;
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let _ = self.tx.send(SyncEvent::Started);
        if let Err(error) = self.request_pass(&client) {
            self.running.store(false, Ordering::Release);
            let _ = self.tx.send(SyncEvent::Failed {
                kind: SyncFailureKind::Problem,
                message: format!("could not start synchronization worker: {error}"),
            });
            return false;
        }
        true
    }

    /// Hands one pass to the long-lived worker, starting a replacement when an
    /// earlier worker stopped, so a pass that panics cannot end synchronization
    /// for the rest of the session.
    fn request_pass(&self, client: &TodoistClient) -> std::result::Result<(), String> {
        let mut passes = self.passes.borrow_mut();
        if let Some(sender) = passes.as_ref()
            && sender.send(()).is_ok()
        {
            return Ok(());
        }
        *passes = None;
        let sender = self
            .spawn_worker(client)
            .map_err(|error| error.to_string())?;
        sender
            .send(())
            .map_err(|_| "worker stopped before its first pass".to_owned())?;
        *passes = Some(sender);
        Ok(())
    }

    fn spawn_worker(&self, client: &TodoistClient) -> std::io::Result<Sender<()>> {
        let (sender, passes) = mpsc::channel();
        let worker = Worker {
            client: client.clone(),
            store: self.store.clone(),
            running: self.running.clone(),
            tx: self.tx.clone(),
            retry_attempts: self.retry_attempts.clone(),
            retry_tx: self.retry_tx.clone(),
            auxiliary_due: self.auxiliary_due.clone(),
            filter_refresh_requested: self.filter_refresh_requested.clone(),
            preferences: self.preferences.clone(),
        };
        thread::Builder::new()
            .name("todoist-tui-sync".into())
            .spawn(move || {
                // Ends once the manager is dropped and closes its sender.
                while passes.recv().is_ok() {
                    worker.run();
                }
            })?;
        #[cfg(test)]
        self.workers_started.set(self.workers_started.get() + 1);
        Ok(sender)
    }

    /// How many worker threads this manager has started. Passes must share one.
    #[cfg(test)]
    pub fn workers_started(&self) -> u32 {
        self.workers_started.get()
    }

    pub fn next_event(&self) -> Option<SyncEvent> {
        self.rx.try_recv().ok()
    }

    #[cfg(test)]
    pub fn send_test_event(&self, event: SyncEvent) {
        self.tx.send(event).unwrap();
    }

    pub fn retry_after(&self) -> Duration {
        self.retry_state()
            .deadline
            .saturating_duration_since(Instant::now())
    }

    pub fn retry_due(&self) -> bool {
        let retry = self.retry_state();
        retry.attempts > 0 && Instant::now() >= retry.deadline
    }

    fn retry_state(&self) -> RetryState {
        for update in self.retry_rx.try_iter() {
            self.retry.set(update);
        }
        self.retry.get()
    }

    #[cfg(test)]
    pub fn with_test_client(store: Store, client: TodoistClient) -> Self {
        let mut manager = Self::new(store, None).unwrap();
        manager.client = Some(client);
        manager
            .store
            .set_meta(
                "completed_history_refresh",
                &chrono::Utc::now().to_rfc3339(),
            )
            .unwrap();
        *manager.auxiliary_due.lock().unwrap() = Instant::now() + Duration::from_secs(3600);
        manager
    }
}

struct Worker {
    client: TodoistClient,
    store: Store,
    running: Arc<AtomicBool>,
    tx: Sender<SyncEvent>,
    retry_attempts: Arc<AtomicU32>,
    retry_tx: Sender<RetryState>,
    auxiliary_due: Arc<Mutex<Instant>>,
    filter_refresh_requested: Arc<AtomicBool>,
    preferences: Arc<Mutex<FilterPreference>>,
}

impl Worker {
    fn run(&self) {
        let mut running_guard = RunningGuard {
            running: self.running.clone(),
            finished: false,
        };
        let mut queued_rejection = false;
        match self.pass(Instant::now(), &mut queued_rejection) {
            Ok(pending) => {
                self.retry_attempts.store(0, Ordering::Release);
                let _ = self.store.set_meta("retry_not_before", "");
                let _ = self.retry_tx.send(RetryState {
                    attempts: 0,
                    deadline: Instant::now(),
                });
                running_guard.finish();
                let _ = self.tx.send(SyncEvent::Finished { pending });
            }
            Err(error) => {
                // Rejections stay durable but do not delay a correction.
                // Transport/server failures back off without sleeping in
                // the worker, so local changes remain completely usable.
                if queued_rejection {
                    self.retry_attempts.store(0, Ordering::Release);
                    let _ = self.store.set_meta("retry_not_before", "");
                    let _ = self.retry_tx.send(RetryState {
                        attempts: 0,
                        deadline: Instant::now(),
                    });
                } else {
                    let attempts = self
                        .retry_attempts
                        .load(Ordering::Acquire)
                        .saturating_add(1);
                    self.retry_attempts.store(attempts, Ordering::Release);
                    let delay = retry_delay(&error).unwrap_or_else(|| {
                        Duration::from_secs((2_u64.pow(attempts.min(8))).min(300))
                    });
                    let delay = delay.max(Duration::from_secs(1));
                    let deadline = Instant::now()
                        .checked_add(delay)
                        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
                    let persisted_delay =
                        chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::days(1));
                    let until = chrono::Utc::now()
                        .checked_add_signed(persisted_delay)
                        .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::days(1));
                    let _ = self.store.set_meta("retry_not_before", &until.to_rfc3339());
                    let _ = self.retry_tx.send(RetryState { attempts, deadline });
                }
                running_guard.finish();
                let _ = self.tx.send(SyncEvent::Failed {
                    kind: classify_failure(&error),
                    message: format!("{error:#}"),
                });
            }
        }
    }

    fn pass(&self, refresh_epoch: Instant, queued_rejection: &mut bool) -> anyhow::Result<usize> {
        self.client.verify_account(&self.store)?;
        // Refetch after every operation because Quick Add replaces temporary IDs
        // in all later entries atomically.
        loop {
            let Some(entry) = self.store.claim_next_outbox()? else {
                break;
            };
            if let Err(error) = self.client.push(&self.store, &entry) {
                if permanent_rejection(&error) {
                    self.store.fail_outbox(entry.seq, &format!("{error:#}"))?;
                } else {
                    return Err(error);
                }
            } else {
                // Retain invalidation if a later push/pull fails;
                // the retry may otherwise have an empty outbox.
                self.filter_refresh_requested.store(true, Ordering::Release);
            }
        }
        self.client.pull(&self.store)?;
        let _ = self.tx.send(SyncEvent::DataRefreshed);
        let uploads_waiting = self.store.has_uploadable_changes()?;
        let requested =
            !uploads_waiting && self.filter_refresh_requested.swap(false, Ordering::AcqRel);
        if !uploads_waiting
            && (requested
                || Instant::now()
                    >= *self
                        .auxiliary_due
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()))
        {
            let result = (|| -> anyhow::Result<AuxiliaryRefresh> {
                let preferred = self
                    .preferences
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .latest();
                let preferred = preferred
                    .map(|id| {
                        self.store
                            .filter(&id)
                            .map(|filter| filter.map(|filter| filter.id))
                    })
                    .transpose()?
                    .flatten();
                let refresh = self.client.pull_auxiliary_with_progress(
                    &self.store,
                    preferred.as_deref(),
                    || self.filter_refresh_requested.load(Ordering::Acquire),
                    |id| {
                        let _ = self.tx.send(SyncEvent::FilterRefreshed {
                            filter_id: id.into(),
                        });
                    },
                )?;
                self.store
                    .set_meta("auxiliary_warning", &refresh.warnings.join("\n\n"))?;
                Ok(refresh)
            })();
            if result.is_err() {
                // Retry a rate-limited/failed explicit refresh after
                // backoff, even if the periodic deadline is later.
                self.filter_refresh_requested.store(true, Ordering::Release);
            }
            let refresh = result?;
            // Only idle polling is minute-throttled. Successful
            // local writes and explicit refreshes bypass this timer.
            if refresh.filters_complete {
                *self
                    .auxiliary_due
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    refresh_epoch + Duration::from_secs(60);
            } else {
                self.filter_refresh_requested.store(true, Ordering::Release);
            }
            if !refresh.warnings.is_empty() {
                let _ = self.tx.send(SyncEvent::AuxiliaryWarning {
                    message: refresh.warnings.join("\n\n"),
                });
            }
        }
        if let Some(message) = self.store.outbox_error()? {
            *queued_rejection = true;
            anyhow::bail!("{message}");
        }
        self.store.outbox_len()
    }
}

fn classify_failure(error: &anyhow::Error) -> SyncFailureKind {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_connect() || error.is_timeout())
    }) {
        SyncFailureKind::Offline
    } else {
        SyncFailureKind::Problem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_transport_failures_are_actionable_problems() {
        let error = anyhow::anyhow!("database is read-only");
        assert_eq!(classify_failure(&error), SyncFailureKind::Problem);
    }

    #[test]
    fn running_guard_releases_the_worker_flag_on_unwind_paths() {
        let running = Arc::new(AtomicBool::new(true));
        {
            let _guard = RunningGuard {
                running: running.clone(),
                finished: false,
            };
        }
        assert!(!running.load(Ordering::Acquire));
    }

    #[test]
    fn ui_sync_methods_do_not_wait_for_worker_owned_state_and_keep_the_latest_preference() {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SyncManager::new(Store::open(dir.path().join("cache.db")).unwrap(), None).unwrap();
        let preferences = manager.preferences.clone();
        let held = preferences.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let ui = thread::spawn(move || {
            manager.prefer_filter(Some("first"));
            manager.prefer_filter(None);
            manager.prefer_filter(Some("latest"));
            assert!(manager.retry_after().is_zero());
            assert!(!manager.retry_due());
            assert!(!manager.start());
            tx.send(manager).unwrap();
        });
        // The simulated worker remains blocked/paused with its lock held until
        // after all UI methods have returned. Always release it before joining
        // so a regression fails rather than hanging the test process.
        let result = rx.recv_timeout(Duration::from_secs(1));
        drop(held);
        ui.join().unwrap();
        let manager = result.expect("UI methods waited for synchronization worker state");
        let mut state = manager.preferences.lock().unwrap();
        assert_eq!(state.latest().as_deref(), Some("latest"));
        assert_eq!(state.latest().as_deref(), Some("latest"));
    }

    #[test]
    fn retry_snapshots_publish_backoff_and_recovery_without_ui_locks() {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SyncManager::new(Store::open(dir.path().join("cache.db")).unwrap(), None).unwrap();
        manager
            .retry_tx
            .send(RetryState {
                attempts: 2,
                deadline: Instant::now() + Duration::from_secs(600),
            })
            .unwrap();
        let remaining = manager.retry_after();
        assert!(remaining > Duration::from_secs(590));
        assert!(remaining <= Duration::from_secs(600));
        assert!(!manager.retry_due());
        manager
            .retry_tx
            .send(RetryState {
                attempts: 2,
                deadline: Instant::now(),
            })
            .unwrap();
        assert!(manager.retry_due());
        assert!(manager.retry_after().is_zero());
        manager
            .retry_tx
            .send(RetryState {
                attempts: 0,
                deadline: Instant::now(),
            })
            .unwrap();
        assert!(!manager.retry_due());
    }
}
