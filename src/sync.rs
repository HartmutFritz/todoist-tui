use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use crate::{store::Store, todoist::TodoistClient};

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
        Ok(Self {
            client: token.map(TodoistClient::new).transpose()?,
            store,
            running: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
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
        let _ = tx.send(SyncEvent::Started);
        let worker = thread::Builder::new()
            .name("todoist-tui-sync".into())
            .spawn(move || {
                let mut running_guard = RunningGuard {
                    running,
                    finished: false,
                };
                let result = (|| -> anyhow::Result<usize> {
                    // Refetch after every operation because Quick Add replaces temporary IDs
                    // in all later entries atomically.
                    loop {
                        let Some(entry) = store.next_outbox()? else {
                            break;
                        };
                        client.push(&store, &entry)?;
                    }
                    client.pull(&store)?;
                    store.outbox_len()
                })();
                running_guard.finish();
                match result {
                    Ok(pending) => {
                        let _ = tx.send(SyncEvent::Finished { pending });
                    }
                    Err(error) => {
                        let _ = tx.send(SyncEvent::Failed {
                            kind: classify_failure(&error),
                            message: error.to_string(),
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
