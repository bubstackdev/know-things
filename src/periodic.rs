//! One opt-in periodic maintenance worker with no queued backlog.
use crate::{contracts::*, store::IngestReport};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Task {
    Reconcile(String),
    Retention(String),
}
impl Task {
    pub(crate) fn collection(&self) -> &str {
        match self {
            Self::Reconcile(id) | Self::Retention(id) => id,
        }
    }
    fn counter(&self) -> &str {
        match self {
            Self::Reconcile(_) => "completedScans",
            Self::Retention(_) => "completedPasses",
        }
    }
    fn operation(&self) -> &str {
        match self {
            Self::Reconcile(_) => "reconcile",
            Self::Retention(_) => "prune",
        }
    }
}
pub(crate) enum Report {
    Reconcile(IngestReport),
    Retention(Value),
}
#[derive(Default)]
pub(crate) struct Config {
    pub tasks: Vec<(Task, u64)>,
    pub watches: Vec<crate::watch::Config>,
}

#[derive(Default)]
struct State {
    epoch: u64,
    configured: usize,
    configured_retention: usize,
    active: Option<Task>,
    last_error: Option<String>,
    attempts: BTreeMap<Task, Value>,
    watches: Value,
}
#[derive(Default)]
pub(crate) struct Control {
    state: Mutex<State>,
    changed: Condvar,
    stop: AtomicBool,
}
impl Control {
    pub(crate) fn wake(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.epoch = state.epoch.wrapping_add(1);
        }
        self.changed.notify_all();
    }
    pub(crate) fn status(&self) -> Value {
        match self.state.lock() {
            Ok(state) => {
                let scans: BTreeMap<_, _> = state
                    .attempts
                    .iter()
                    .filter_map(|(task, value)| match task {
                        Task::Reconcile(id) => Some((id, value)),
                        _ => None,
                    })
                    .collect();
                let retention: BTreeMap<_, _> = state
                    .attempts
                    .iter()
                    .filter_map(|(task, value)| match task {
                        Task::Retention(id) => Some((id, value)),
                        _ => None,
                    })
                    .collect();
                json!({"configuredCollections":state.configured,"configuredRetentionCollections":state.configured_retention,"activeCollection":state.active.as_ref().map(Task::collection),"activeOperation":state.active.as_ref().map(Task::operation),"lastErrorCode":state.last_error,"collections":scans,"retention":retention,"watch":state.watches})
            }
            Err(_) => json!({"lastErrorCode":"worker_failed"}),
        }
    }
}
pub(crate) struct Owner {
    control: Arc<Control>,
    join: Option<JoinHandle<()>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        // Serialize stop with the predicate check before the indefinite idle
        // wait, so a shutdown notification cannot be lost in that transition.
        let state = self.control.state.lock();
        self.control.stop.store(true, Ordering::SeqCst);
        self.control.changed.notify_all();
        drop(state);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
pub(crate) fn start(
    control: Arc<Control>,
    load: impl Fn() -> Result<Config> + Send + 'static,
    run: impl Fn(&Task, &AtomicBool) -> Result<Report> + Send + 'static,
) -> Arc<Owner> {
    let worker = control.clone();
    let join = std::thread::spawn(move || {
        let mut due = BTreeMap::<Task, (u64, Instant, bool)>::new();
        let mut watches = crate::watch::Watches::default();
        loop {
            if worker.stop.load(Ordering::SeqCst) {
                break;
            }
            let epoch = match worker.state.lock() {
                Ok(s) => s.epoch,
                Err(_) => break,
            };
            let config = match load() {
                Ok(configs) => configs,
                Err(error) => {
                    if error.code == "canceled" {
                        break;
                    }
                    let Ok(mut state) = worker.state.lock() else {
                        break;
                    };
                    state.last_error = Some(error.code);
                    if worker.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = worker.changed.wait_timeout(state, Duration::from_secs(1));
                    continue;
                }
            };
            let configs = config.tasks;
            let hints = watches.sync(&config.watches);
            due.retain(|id, _| configs.iter().any(|(collection, _)| collection == id));
            for (id, interval) in &configs {
                due.entry(id.clone())
                    .and_modify(|entry| {
                        if entry.0 != *interval {
                            *entry = (
                                *interval,
                                Instant::now() + Duration::from_millis(*interval),
                                false,
                            );
                        }
                    })
                    .or_insert((
                        *interval,
                        Instant::now() + Duration::from_millis(*interval),
                        false,
                    ));
            }
            for collection in hints {
                if let Some(entry) = due.get_mut(&Task::Reconcile(collection))
                    && !entry.2
                {
                    entry.1 = Instant::now();
                }
            }
            let next = due
                .iter()
                .min_by_key(|(_, entry)| entry.1)
                .map(|(id, (ms, at, _))| (id.clone(), *ms, *at));
            let Ok(mut state) = worker.state.lock() else {
                break;
            };
            state.configured = configs
                .iter()
                .filter(|(task, _)| matches!(task, Task::Reconcile(_)))
                .count();
            state.configured_retention = configs.len() - state.configured;
            state.watches = watches.status();
            state.attempts.retain(|id, _| due.contains_key(id));
            if worker.stop.load(Ordering::SeqCst) {
                break;
            }
            if state.epoch != epoch {
                continue;
            }
            if next.is_none() {
                drop(worker.changed.wait_while(state, |s| {
                    s.epoch == epoch && !worker.stop.load(Ordering::SeqCst)
                }));
                continue;
            }
            let (id, interval, at) = next.unwrap();
            if Instant::now() < at {
                drop(worker.changed.wait_timeout_while(
                    state,
                    at.saturating_duration_since(Instant::now()).min(
                        if config.watches.is_empty() {
                            Duration::MAX
                        } else {
                            Duration::from_millis(crate::watch::POLL_MS)
                        },
                    ),
                    |s| s.epoch == epoch && !worker.stop.load(Ordering::SeqCst),
                ));
                continue;
            }
            state.active = Some(id.clone());
            drop(state);
            let started = Instant::now();
            let result = run(&id, &worker.stop);
            let busy = result.as_ref().is_err_and(|e| e.code == "busy");
            due.insert(
                id.clone(),
                (
                    interval,
                    Instant::now() + Duration::from_millis(if busy { 1000 } else { interval }),
                    busy,
                ),
            );
            if let Ok(mut state) = worker.state.lock() {
                state.active = None;
                let prior = state.attempts.get(&id);
                let completed = prior.and_then(|v| v[id.counter()].as_u64()).unwrap_or(0);
                let skipped = prior.and_then(|v| v["busySkips"].as_u64()).unwrap_or(0);
                let (error, summary) = match result {
                    Ok(Report::Reconcile(report)) => (
                        if report.failures.is_empty() {
                            None
                        } else {
                            Some("ingestion_failures".to_owned())
                        },
                        Some(
                            json!({"active":report.active,"unchanged":report.unchanged,"excluded":report.excluded,"filteredEntries":report.filtered_entries,"failures":report.failures.len()}),
                        ),
                    ),
                    Ok(Report::Retention(report)) => (None, Some(report)),
                    Err(error) => (Some(error.code), None),
                };
                state.last_error = error.clone();
                let mut metadata = json!({"busySkips":skipped.saturating_add(u64::from(busy)),"lastErrorCode":error,"lastDurationMs":started.elapsed().as_secs_f64()*1000.,"lastReport":summary});
                metadata[id.counter()] =
                    json!(completed.saturating_add(u64::from(metadata["lastReport"].is_object())));
                state.attempts.insert(id, metadata);
            }
        }
    });
    Arc::new(Owner {
        control,
        join: Some(join),
    })
}
