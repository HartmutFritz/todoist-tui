use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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
    retry: Arc<Mutex<RetryState>>,
    auxiliary_due: Arc<Mutex<Instant>>,
    filter_refresh_requested: Arc<AtomicBool>,
    preferred_filter: Arc<Mutex<Option<String>>>,
}

struct RetryState {
    attempts: u32,
    deadline: Instant,
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
            retry: Arc::new(Mutex::new(RetryState {
                attempts: u32::from(!retry_after.is_zero()),
                deadline: Instant::now()
                    .checked_add(retry_after)
                    .unwrap_or_else(Instant::now),
            })),
            auxiliary_due: Arc::new(Mutex::new(Instant::now())),
            filter_refresh_requested: Arc::new(AtomicBool::new(false)),
            preferred_filter: Arc::new(Mutex::new(None)),
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
        let mut preferred = self
            .preferred_filter
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if preferred.as_deref() != id {
            *preferred = id.map(str::to_owned);
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
        let store = self.store.clone();
        let running = self.running.clone();
        let tx = self.tx.clone();
        let retry = self.retry.clone();
        let auxiliary_due = self.auxiliary_due.clone();
        let filter_refresh_requested = self.filter_refresh_requested.clone();
        let preferred_filter = self.preferred_filter.clone();
        let refresh_epoch = Instant::now();
        let _ = tx.send(SyncEvent::Started);
        let worker = thread::Builder::new()
            .name("todoist-tui-sync".into())
            .spawn(move || {
                let mut running_guard = RunningGuard {
                    running,
                    finished: false,
                };
                let mut queued_rejection = false;
                let result = (|| -> anyhow::Result<usize> {
                    client.verify_account(&store)?;
                    // Refetch after every operation because Quick Add replaces temporary IDs
                    // in all later entries atomically.
                    loop {
                        let Some(entry) = store.claim_next_outbox()? else {
                            break;
                        };
                        if let Err(error) = client.push(&store, &entry) {
                            if permanent_rejection(&error) {
                                store.fail_outbox(entry.seq, &format!("{error:#}"))?;
                            } else {
                                return Err(error);
                            }
                        } else {
                            // Retain invalidation if a later push/pull fails;
                            // the retry may otherwise have an empty outbox.
                            filter_refresh_requested.store(true, Ordering::Release);
                        }
                    }
                    client.pull(&store)?;
                    let _ = tx.send(SyncEvent::DataRefreshed);
                    let uploads_waiting = store.has_uploadable_changes()?;
                    let requested =
                        !uploads_waiting && filter_refresh_requested.swap(false, Ordering::AcqRel);
                    if !uploads_waiting
                        && (requested
                            || Instant::now()
                                >= *auxiliary_due
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner()))
                    {
                        let result = (|| -> anyhow::Result<AuxiliaryRefresh> {
                            let preferred = preferred_filter
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .clone();
                            let preferred = preferred
                                .map(|id| {
                                    store
                                        .filter(&id)
                                        .map(|filter| filter.map(|filter| filter.id))
                                })
                                .transpose()?
                                .flatten();
                            let refresh = client.pull_auxiliary_with_progress(
                                &store,
                                preferred.as_deref(),
                                || filter_refresh_requested.load(Ordering::Acquire),
                                |id| {
                                    let _ = tx.send(SyncEvent::FilterRefreshed {
                                        filter_id: id.into(),
                                    });
                                },
                            )?;
                            store.set_meta("auxiliary_warning", &refresh.warnings.join("\n\n"))?;
                            Ok(refresh)
                        })();
                        if result.is_err() {
                            // Retry a rate-limited/failed explicit refresh after
                            // backoff, even if the periodic deadline is later.
                            filter_refresh_requested.store(true, Ordering::Release);
                        }
                        let refresh = result?;
                        // Only idle polling is minute-throttled. Successful
                        // local writes and explicit refreshes bypass this timer.
                        if refresh.filters_complete {
                            *auxiliary_due
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) =
                                refresh_epoch + Duration::from_secs(60);
                        } else {
                            filter_refresh_requested.store(true, Ordering::Release);
                        }
                        if !refresh.warnings.is_empty() {
                            let _ = tx.send(SyncEvent::AuxiliaryWarning {
                                message: refresh.warnings.join("\n\n"),
                            });
                        }
                    }
                    if let Some(message) = store.outbox_error()? {
                        queued_rejection = true;
                        anyhow::bail!("{message}");
                    }
                    store.outbox_len()
                })();
                match result {
                    Ok(pending) => {
                        let mut retry = retry.lock().unwrap_or_else(|error| error.into_inner());
                        retry.attempts = 0;
                        retry.deadline = Instant::now();
                        let _ = store.set_meta("retry_not_before", "");
                        drop(retry);
                        running_guard.finish();
                        let _ = tx.send(SyncEvent::Finished { pending });
                    }
                    Err(error) => {
                        // Rejections stay durable but do not delay a correction.
                        // Transport/server failures back off without sleeping in
                        // the worker, so local changes remain completely usable.
                        if !queued_rejection {
                            let mut retry = retry.lock().unwrap_or_else(|error| error.into_inner());
                            retry.attempts = retry.attempts.saturating_add(1);
                            let delay = retry_delay(&error).unwrap_or_else(|| {
                                Duration::from_secs((2_u64.pow(retry.attempts.min(8))).min(300))
                            });
                            let delay = delay.max(Duration::from_secs(1));
                            retry.deadline = Instant::now()
                                .checked_add(delay)
                                .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
                            let persisted_delay = chrono::Duration::from_std(delay)
                                .unwrap_or(chrono::Duration::days(1));
                            let until = chrono::Utc::now()
                                .checked_add_signed(persisted_delay)
                                .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::days(1));
                            let _ = store.set_meta("retry_not_before", &until.to_rfc3339());
                        } else {
                            let mut retry = retry.lock().unwrap_or_else(|error| error.into_inner());
                            retry.attempts = 0;
                            retry.deadline = Instant::now();
                            let _ = store.set_meta("retry_not_before", "");
                        }
                        running_guard.finish();
                        let _ = tx.send(SyncEvent::Failed {
                            kind: classify_failure(&error),
                            message: format!("{error:#}"),
                        });
                    }
                }
            });
        if let Err(error) = worker {
            self.running.store(false, Ordering::Release);
            let _ = self.tx.send(SyncEvent::Failed {
                kind: SyncFailureKind::Problem,
                message: format!("could not start synchronization worker: {error}"),
            });
            return false;
        }
        true
    }

    pub fn next_event(&self) -> Option<SyncEvent> {
        self.rx.try_recv().ok()
    }

    pub fn retry_after(&self) -> Duration {
        self.retry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .deadline
            .saturating_duration_since(Instant::now())
    }

    pub fn retry_due(&self) -> bool {
        let retry = self.retry.lock().unwrap_or_else(|error| error.into_inner());
        retry.attempts > 0 && Instant::now() >= retry.deadline
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
}
