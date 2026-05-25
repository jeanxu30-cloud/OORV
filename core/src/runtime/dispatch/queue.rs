use std::cell::RefCell;
use std::cmp::{Ordering, Reverse};
use std::rc::Rc;
use std::time::Duration;

use crate::oorvir::refined::{Deadline, Stream, Task, OORVIR};
use priority_queue::PriorityQueue;

use crate::runtime::Value;

/// Describes a single unit of evaluation work.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum WorkItem {
    /// Evaluate a specific parameterised instance of an output stream.
    Compute(usize, Vec<Value>),
    /// Evaluate all active instances of an output stream.
    ComputeAll(usize),
    /// Activate (start) a new instance of an output stream.
    Activate(usize),
    /// Evaluate the end condition for a specific parameterised instance.
    Deactivate(usize, Vec<Value>),
    /// Evaluate the end condition for all active instances of an output stream.
    DeactivateAll(usize),
}

impl From<Task> for WorkItem {
    fn from(task: Task) -> Self {
        match task {
            Task::Evaluate(idx) => WorkItem::ComputeAll(idx),
            Task::Start(idx) => WorkItem::Activate(idx),
            Task::End(idx) => WorkItem::DeactivateAll(idx),
        }
    }
}

impl WorkItem {
    /// Return the dependency level used to sort this item within a batch.
    ///
    /// Lower values must be evaluated before higher ones.
    pub(crate) fn priority_level(&self, ir: &OORVIR) -> usize {
        match self {
            WorkItem::Compute(idx, _) | WorkItem::ComputeAll(idx) => {
                ir.constraints[*idx].eval_layer().inner()
            }
            WorkItem::Activate(idx) => ir.constraints[*idx].start_layer().inner(),
            WorkItem::Deactivate(_, _) | WorkItem::DeactivateAll(_) => usize::MAX,
        }
    }
}

/// A work item paired with the period at which it should recur.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RecurringItem {
    /// The work to perform when this entry is due.
    item: WorkItem,
    /// How long to wait before this entry is due again.
    period: Duration,
}

/// A group of work items that all share the same due Duration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DueWorkBatch {
    /// When (relative to monitor start) this batch is due.
    pub(crate) due: Duration,
    /// The work items in this batch, in evaluation order.
    pub(crate) items: Vec<WorkItem>,
}

impl DueWorkBatch {
    /// Sort the batch items by their dependency level (lowest first).
    pub(crate) fn sort_by_priority(&mut self, ir: &OORVIR) {
        self.items.sort_by_key(|w| w.priority_level(ir));
    }
}

/// Priority-queue-based dynamic scheduler.
///
/// Items are stored with a `Reverse<Duration>` priority so that the earliest due
/// Duration is always at the front of the queue.
#[derive(Debug, Clone)]
pub(crate) struct EventQueue {
    queue: PriorityQueue<RecurringItem, Reverse<Duration>>,
}

impl EventQueue {
    /// Create an empty `EventQueue`.
    pub(crate) fn new() -> Self {
        EventQueue {
            queue: PriorityQueue::new(),
        }
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Insert a `RecurringItem` scheduled to fire at `now + period`.
    fn enqueue(&mut self, entry: RecurringItem, now: Duration) {
        let due = now + entry.period;
        self.queue.push(entry, Reverse(due));
    }

    /// Remove a specific `RecurringItem` from the queue (if present).
    fn cancel(&mut self, entry: &RecurringItem) {
        self.queue.remove(entry);
    }

    // ── Public interface ─────────────────────────────────────────────────────

    /// Schedule periodic evaluation of a stream instance.
    pub(crate) fn enqueue_compute(
        &mut self,
        target: usize,
        params: &[Value],
        now: Duration,
        period: Duration,
    ) {
        self.enqueue(
            RecurringItem {
                item: WorkItem::Compute(target, params.to_vec()),
                period,
            },
            now,
        );
    }

    /// Schedule periodic end-condition evaluation for a stream instance.
    pub(crate) fn enqueue_end(
        &mut self,
        target: usize,
        params: &[Value],
        now: Duration,
        period: Duration,
    ) {
        self.enqueue(
            RecurringItem {
                item: WorkItem::Deactivate(target, params.to_vec()),
                period,
            },
            now,
        );
    }

    /// Remove a previously-scheduled compute entry.
    pub(crate) fn cancel_compute(&mut self, target: usize, params: &[Value], period: Duration) {
        self.cancel(&RecurringItem {
            item: WorkItem::Compute(target, params.to_vec()),
            period,
        });
    }

    /// Remove a previously-scheduled end-condition entry.
    pub(crate) fn cancel_end(&mut self, target: usize, params: &[Value], period: Duration) {
        self.cancel(&RecurringItem {
            item: WorkItem::Deactivate(target, params.to_vec()),
            period,
        });
    }

    /// Pop all items due at or before `now`, reschedule them, and return a [`DueWorkBatch`].
    ///
    /// Returns `None` when nothing is due yet.
    pub(crate) fn pop_batch_due_at(&mut self, now: Duration) -> Option<DueWorkBatch> {
        if self.queue.peek().map_or(true, |(_, Reverse(t))| *t > now) {
            return None;
        }

        let (first, Reverse(batch_due)) = self.queue.pop().unwrap();
        self.queue
            .push(first.clone(), Reverse(batch_due + first.period));

        let mut items = vec![first.item];

        while self
            .queue
            .peek()
            .map_or(false, |(_, Reverse(t))| *t == batch_due)
        {
            let (entry, Reverse(t)) = self.queue.pop().unwrap();
            self.queue.push(entry.clone(), Reverse(t + entry.period));
            items.push(entry.item);
        }

        Some(DueWorkBatch {
            due: batch_due,
            items,
        })
    }

    /// Peek at the Duration of the next batch without removing anything.
    ///
    /// Returns `None` when the queue is empty.
    pub(crate) fn peek_next_due_time(&self) -> Option<Duration> {
        self.queue.peek().map(|(_, Reverse(t))| *t)
    }
}

/// Manages both the static (periodic) schedule and the dynamic event queue.
///
/// The static schedule is built once from the compiled spec during [`build`](TaskCoordinator::build).
/// The dynamic queue is shared with the evaluator and updated at runtime.
pub(crate) struct TaskCoordinator {
    ir: OORVIR,
    /// Whether the spec contains any periodically-driven streams.
    has_periodic: bool,
    /// The full static deadline table, in cyclic order.
    static_deadlines: Vec<Deadline>,
    /// Shared mutable dynamic scheduler.
    event_queue: Rc<RefCell<EventQueue>>,
    /// Current position in the static deadline cycle.
    deadline_cursor: usize,
    /// Wall-clock Duration when the next static deadline fires.
    next_periodic_due: Option<Duration>,
    /// Work items belonging to the current static deadline slot.
    pending_periodic: Vec<Task>,
}

impl TaskCoordinator {
    /// Construct a `TaskCoordinator` from the compiled spec and a shared [`EventQueue`].
    ///
    /// Returns an error string when `ir.build_schedule()` fails.
    pub(crate) fn build(
        ir: OORVIR,
        event_queue: Rc<RefCell<EventQueue>>,
    ) -> Result<TaskCoordinator, String> {
        if !ir.has_periodic_streams() {
            return Ok(TaskCoordinator {
                ir,
                has_periodic: false,
                static_deadlines: vec![],
                event_queue,
                deadline_cursor: 0,
                next_periodic_due: None,
                pending_periodic: vec![],
            });
        }

        let schedule = ir.build_schedule()?;
        let (pending, first_due) = match schedule.deadlines.first() {
            None => (vec![], None),
            Some(d) => (d.due.clone(), Some(d.pause)),
        };

        Ok(TaskCoordinator {
            ir,
            has_periodic: true,
            static_deadlines: schedule.deadlines,
            event_queue,
            deadline_cursor: 0,
            next_periodic_due: first_due,
            pending_periodic: pending,
        })
    }

    /// Return the soonest upcoming deadline from either the static or dynamic schedule.
    pub(crate) fn earliest_due(&self) -> Option<Duration> {
        let dyn_due = self.event_queue.borrow().peek_next_due_time();
        match (self.next_periodic_due, dyn_due) {
            (None, None) => None,
            (Some(t), None) | (None, Some(t)) => Some(t),
            (Some(s), Some(d)) => Some(s.min(d)),
        }
    }

    /// Advance the static deadline cursor by one slot and return the current slot's items.
    fn advance_periodic_deadline(&mut self) -> Vec<WorkItem> {
        debug_assert!(
            !self.pending_periodic.is_empty() && self.next_periodic_due.is_some(),
            "advance_periodic_deadline called when no periodic deadline is pending"
        );

        let items: Vec<WorkItem> = self.pending_periodic.iter().map(|t| (*t).into()).collect();

        self.deadline_cursor = (self.deadline_cursor + 1) % self.static_deadlines.len();
        let next = &self.static_deadlines[self.deadline_cursor];
        assert!(
            next.pause > Duration::ZERO,
            "static deadline pause must be positive"
        );
        self.next_periodic_due = self.next_periodic_due.map(|d| d + next.pause);
        self.pending_periodic.clone_from(&next.due);

        items
    }

    /// Collect and return all work items that are due at the current moment `now`.
    ///
    /// Callers must only invoke this method when [`earliest_due`](TaskCoordinator::earliest_due)
    /// has returned a Duration ?`now`.
    pub(crate) fn collect_due_work(&mut self, now: Duration) -> Vec<WorkItem> {
        debug_assert!(
            self.has_periodic,
            "collect_due_work on non-periodic coordinator"
        );

        let periodic_due = self.next_periodic_due;
        let dynamic_due = self.event_queue.borrow().peek_next_due_time();

        match (periodic_due, dynamic_due) {
            (None, None) => vec![],

            (None, Some(_)) => {
                let mut batch = self
                    .event_queue
                    .borrow_mut()
                    .pop_batch_due_at(now)
                    .expect("dynamic due Duration present but pop_batch_due_at returned None");
                batch.sort_by_priority(&self.ir);
                batch.items
            }

            (Some(_), None) => self.advance_periodic_deadline(),

            (Some(s), Some(d)) => match s.cmp(&d) {
                Ordering::Less => self.advance_periodic_deadline(),

                Ordering::Equal => {
                    let periodic_items = self.advance_periodic_deadline();
                    let dynamic_items = self
                        .event_queue
                        .borrow_mut()
                        .pop_batch_due_at(now)
                        .unwrap()
                        .items;
                    let mut merged: Vec<WorkItem> =
                        periodic_items.into_iter().chain(dynamic_items).collect();
                    merged.sort_by_key(|w| w.priority_level(&self.ir));
                    merged
                }

                Ordering::Greater => {
                    let mut batch =
                        self.event_queue.borrow_mut().pop_batch_due_at(now).expect(
                            "dynamic due Duration present but pop_batch_due_at returned None",
                        );
                    batch.sort_by_priority(&self.ir);
                    batch.items
                }
            },
        }
    }
}
