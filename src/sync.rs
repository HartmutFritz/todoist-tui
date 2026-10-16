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
    todoist::{TodoistClient, permanent_rejection, retry_delay},
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
        })
    }

    pub fn has_token(&self) -> bool {
        self.client.is_some()
    }
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
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
                        }
                    }
                    client.pull(&store)?;
                    if store.outbox_len()? == 0
                        && Instant::now()
                            >= *auxiliary_due
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                    {
                        let warnings = client.pull_auxiliary(&store)?;
                        store.set_meta("auxiliary_warning", &warnings.join("\n\n"))?;
                        // Membership is cheap enough to refresh every minute;
                        // immediate task pushes never wait for another history scan.
                        *auxiliary_due
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) =
                            Instant::now() + Duration::from_secs(60);
                        if !warnings.is_empty() {
                            let _ = tx.send(SyncEvent::AuxiliaryWarning {
                                message: warnings.join("\n\n"),
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
