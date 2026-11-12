//! Publishing state changes as snapshots or deltas.
//!
//! [`StatePublisher`] keeps the baseline the client holds: the input's state
//! until the agent's first snapshot (RUNIN-25, SNAP-06), then whatever was
//! last published. [`StatePublisher::publish`] diffs against it with
//! [`patch::diff`] and returns a `STATE_DELTA`, or a
//! `STATE_SNAPSHOT` when the delta would be larger. A delta is never sent
//! without a baseline (SNAP-07).

use serde_json::Value;

use crate::event::Event;
use crate::patch::{self, PatchBudget, PatchLimits};
use crate::state::{StateDelta, StateSnapshot};

/// What a publication sends.
enum Choice {
    Nothing,
    Snapshot,
    Delta(Vec<patch::PatchOperation>),
}

/// Tracks the client's state baseline.
#[derive(Debug, Clone)]
pub struct StatePublisher {
    baseline: Value,
    /// Incremental node budget for applying observed deltas.
    budget: Option<PatchBudget>,
    /// Set when an observed delta did not apply: the client's copy can no
    /// longer be vouched for, so the next publication is a snapshot (SNAP-04).
    diverged: bool,
}

/// As [`StatePublisher::new`] over an absent input state: baseline `{}`.
impl Default for StatePublisher {
    fn default() -> Self {
        Self::new(Value::Null)
    }
}

impl StatePublisher {
    /// A publisher whose baseline is the input's state (`{}` when absent).
    #[must_use]
    pub fn new(input_state: Value) -> Self {
        let baseline = if input_state.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            input_state
        };
        Self {
            baseline,
            budget: None,
            diverged: false,
        }
    }

    /// The event bringing the client from the baseline to `next`, if they
    /// differ. Updates the baseline.
    pub fn publish(&mut self, next: &Value) -> Option<Event> {
        let event = self.prepare(next)?;
        self.observe(&event);
        Some(event)
    }

    /// A full snapshot of `current`, resetting the baseline (SNAP-15).
    pub fn snapshot(&mut self, current: &Value) -> Event {
        let event = self.snapshot_event(current.clone());
        self.observe(&event);
        event
    }

    /// Keeps the baseline aligned with state events the agent emitted directly.
    pub fn observe(&mut self, event: &Event) {
        match event {
            Event::StateSnapshot(s) => {
                self.baseline = s.snapshot.clone();
                self.budget = None;
                self.diverged = false;
            }
            Event::StateDelta(d) => {
                let baseline = &mut self.baseline;
                let budget = self.budget.get_or_insert_with(|| {
                    PatchBudget::for_document(baseline, PatchLimits::default())
                });
                if patch::apply(baseline, &d.delta, budget).is_err() {
                    self.diverged = true;
                }
            }
            _ => {}
        }
    }

    /// The event for `next` without committing it: the runner commits what
    /// the client actually receives, after transformers, via [`observe`](Self::observe).
    pub(crate) fn prepare(&self, next: &Value) -> Option<Event> {
        match self.choose(next) {
            Choice::Nothing => None,
            Choice::Snapshot => Some(self.snapshot_event(next.clone())),
            Choice::Delta(delta) => Some(StateDelta::new(delta).into()),
        }
    }

    /// As [`prepare`](Self::prepare), taking `next` so a snapshot moves it
    /// instead of copying it.
    pub(crate) fn prepare_owned(&self, next: Value) -> Option<Event> {
        match self.choose(&next) {
            Choice::Nothing => None,
            Choice::Snapshot => Some(self.snapshot_event(next)),
            Choice::Delta(delta) => Some(StateDelta::new(delta).into()),
        }
    }

    fn choose(&self, next: &Value) -> Choice {
        if self.diverged {
            return Choice::Snapshot;
        }
        // The budget, once a delta was observed, already holds the baseline's
        // node count: no need to walk it again.
        let nodes = self.budget.as_ref().map(PatchBudget::nodes);
        // A delta the default patch limits would refuse (where `diff` falls
        // back to a root `replace`, always larger than `next`) is a snapshot.
        let Some(delta) = patch::diff_or_none(&self.baseline, nodes, next) else {
            return Choice::Snapshot;
        };
        if delta.is_empty() {
            return Choice::Nothing;
        }
        // Send whichever is smaller on the wire (ties go to the delta).
        // `next` is only counted as far as the delta's size.
        let delta_bytes = crate::json::wire_bytes(&delta);
        if crate::json::wire_bytes_upto(next, delta_bytes) < delta_bytes {
            return Choice::Snapshot;
        }
        Choice::Delta(delta)
    }

    pub(crate) fn snapshot_event(&self, next: Value) -> Event {
        StateSnapshot::new(next).into()
    }
}
