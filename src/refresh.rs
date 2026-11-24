use crate::{
    admin::IndexProgress,
    source,
    timeline::{OperationPhase as Phase, Outcome as State},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use uuid::Uuid;

const HISTORY: usize = 32;

pub struct RefreshRequest {
    pub id: Uuid,
    pub expected_commit: Option<String>,
}

#[derive(Clone)]
pub struct RefreshControl {
    queue: mpsc::Sender<RefreshRequest>,
    progress: Arc<Mutex<RefreshProgress>>,
}

#[derive(Default)]
struct RefreshProgress {
    active: Option<(Uuid, IndexProgress)>,
    completed: VecDeque<(Uuid, IndexProgress)>,
}

impl RefreshControl {
    pub fn new() -> (Self, mpsc::Receiver<RefreshRequest>) {
        let (queue, receiver) = mpsc::channel(1);
        (
            Self {
                queue,
                progress: Arc::default(),
            },
            receiver,
        )
    }

    pub fn request(
        &self,
        id: Uuid,
        expected_commit: Option<String>,
        indexed: Option<String>,
    ) -> IndexProgress {
        let mut progress = self.progress.lock().unwrap();
        if let Some(op) = progress.find(id) {
            return op.clone();
        }
        if expected_commit
            .as_deref()
            .is_some_and(|commit| !source::is_oid(commit))
        {
            return IndexProgress::failed("invalid_commit", indexed);
        }
        if progress.active.is_some() {
            return IndexProgress::failed("operation_busy", indexed);
        }
        let op = IndexProgress {
            phase: Phase::WaitingForIndex,
            state: State::Running,
            target_commit: expected_commit.clone(),
            indexed_commit: indexed.clone(),
            code: None,
        };
        if self
            .queue
            .try_send(RefreshRequest {
                id,
                expected_commit,
            })
            .is_err()
        {
            return IndexProgress::failed("admin_unreachable", indexed);
        }
        progress.active = Some((id, op.clone()));
        op
    }

    pub fn status(&self, id: Uuid, indexed: Option<String>) -> IndexProgress {
        self.progress
            .lock()
            .unwrap()
            .find(id)
            .cloned()
            .unwrap_or_else(|| IndexProgress::failed("operation_not_found", indexed))
    }

    pub fn indexing(&self, id: Uuid, commit: &str) {
        let mut progress = self.progress.lock().unwrap();
        if let Some((active_id, op)) = &mut progress.active
            && *active_id == id
        {
            op.phase = Phase::Indexing;
            op.target_commit = Some(commit.to_owned());
        }
    }

    pub fn finish(
        &self,
        id: Uuid,
        result: Result<Option<&'static str>, &'static str>,
        indexed: Option<String>,
    ) {
        let mut progress = self.progress.lock().unwrap();
        if !progress
            .active
            .as_ref()
            .is_some_and(|(active_id, _)| *active_id == id)
        {
            return;
        }
        let (_, mut op) = progress.active.take().unwrap();
        op.indexed_commit = indexed;
        match result {
            Ok(code) => {
                op.phase = Phase::Done;
                op.state = State::Succeeded;
                op.code = code.map(str::to_owned);
            }
            Err(code) => {
                op.state = State::Failed;
                op.code = Some(code.to_owned());
            }
        }
        progress.completed.push_front((id, op));
        progress.completed.truncate(HISTORY);
    }
}

impl RefreshProgress {
    fn find(&self, id: Uuid) -> Option<&IndexProgress> {
        self.active
            .iter()
            .chain(&self.completed)
            .find_map(|(op_id, op)| (*op_id == id).then_some(op))
    }
}
