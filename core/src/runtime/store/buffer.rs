use std::collections::{HashMap, HashSet, VecDeque};

use crate::oorvir::refined::{ConstraintStream, StorageRequirement, Stream, Type, OORVIR};

use super::Value;

/// Default capacity for unbounded buffers; sized to avoid frequent reallocation.
const DEFAULT_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct ValueBuffer {
    /// Values stored front-to-back; index 0 is the most recent.
    buffer: VecDeque<Value>,
    /// Maximum retained entries (or unbounded).
    bound: StorageRequirement,
    /// Whether this instance is currently active.
    live: bool,
}

impl ValueBuffer {
    /// Allocate a new buffer for a stream instance.
    pub(crate) fn new(_ty: &Type, bound: StorageRequirement, active: bool) -> ValueBuffer {
        let capacity = match bound {
            StorageRequirement::Bounded(limit) => limit as usize,
            StorageRequirement::Unbounded => DEFAULT_CAPACITY,
        };
        ValueBuffer {
            buffer: VecDeque::with_capacity(capacity),
            bound,
            live: active,
        }
    }

    /// Read the value at a non-positive `offset` relative to the current head.
    ///
    /// `offset == 0` returns the most recently written value; negative offsets
    /// look further back. Returns `None` when the instance is inactive or the
    /// offset exceeds the buffer length.
    pub(crate) fn read_at(&self, offset: i16) -> Option<Value> {
        assert!(offset <= 0, "read_at: offset must be <= 0, got {offset}");
        if !self.live {
            return None;
        }
        if offset == 0 {
            self.buffer.front().cloned()
        } else {
            self.buffer.get(offset.unsigned_abs() as usize).cloned()
        }
    }

    /// Prepend `value` to the buffer, evicting the oldest entry when the
    /// bounded capacity is reached.
    pub(crate) fn write(&mut self, value: Value) {
        assert!(
            self.live,
            "write: attempted to write to an inactive ValueBuffer"
        );
        if let StorageRequirement::Bounded(limit) = self.bound {
            if self.buffer.len() == limit as usize {
                self.buffer.pop_back();
            }
        }
        self.buffer.push_front(value);
    }

    /// Mark this instance as active.
    pub(crate) fn enable(&mut self) {
        self.live = true;
    }

    /// Returns `true` if this instance is currently active.
    pub(crate) fn enabled(&self) -> bool {
        self.live
    }

    /// Mark this instance as inactive and discard all buffered values.
    pub(crate) fn disable(&mut self) {
        self.live = false;
        self.buffer.clear();
    }
}

/// Holds every live instance of a single parameterized constraint stream,
/// together with bookkeeping sets that track lifecycle events per evaluation cycle.
pub(crate) struct SlotGroup {
    /// Active instances keyed by their parameter tuple.
    slots: HashMap<Vec<Value>, ValueBuffer>,
    /// Parameters whose buffer received a new value this cycle.
    updated: HashSet<Vec<Value>>,
    /// Parameters that were activated (first seen) this cycle.
    added: HashSet<Vec<Value>>,
    /// Parameters that were ended (last seen) this cycle.
    removed: HashSet<Vec<Value>>,
    /// Element type stored in each buffer.
    element_type: Type,
    /// Retention bound shared by all buffers in this group.
    retention: StorageRequirement,
}

impl SlotGroup {
    /// Create an empty group for a stream with the given element type and storage bound.
    pub(crate) fn new(ty: &Type, bound: StorageRequirement) -> Self {
        SlotGroup {
            slots: HashMap::new(),
            updated: HashSet::new(),
            added: HashSet::new(),
            removed: HashSet::new(),
            element_type: ty.clone(),
            retention: bound,
        }
    }

    /// Return an immutable reference to the buffer for `params`, or `None`.
    pub(crate) fn slot(&self, params: &[Value]) -> Option<&ValueBuffer> {
        self.slots.get(params)
    }

    /// Return a mutable reference to the buffer for `params`, recording the
    /// instance as updated, or `None` if no such instance exists.
    pub(crate) fn slot_mut(&mut self, params: &[Value]) -> Option<&mut ValueBuffer> {
        self.updated.insert(params.to_vec());
        self.slots.get_mut(params)
    }

    /// Register a new instance for `params` if one does not already exist.
    pub(crate) fn register(&mut self, params: &[Value]) -> Option<&ValueBuffer> {
        if !self.slots.contains_key(params) {
            self.added.insert(params.to_vec());
            self.slots.insert(
                params.to_vec(),
                ValueBuffer::new(&self.element_type, self.retention, true),
            );
            self.slots.get(params)
        } else {
            None
        }
    }

    /// Mark the instance identified by `params` for removal at the end of this cycle.
    pub(crate) fn schedule_removal(&mut self, params: &[Value]) {
        debug_assert!(
            self.slots.contains_key(params),
            "schedule_removal: instance not found"
        );
        self.removed.insert(params.to_vec());
    }

    /// Destroy all instances that were scheduled for removal and collect their last values.
    pub(crate) fn flush_removed(&mut self) -> Vec<Value> {
        self.removed
            .iter()
            .filter_map(|key| self.slots.remove(key).and_then(|buf| buf.read_at(0)))
            .collect()
    }

    /// Iterator over all live parameter tuples.
    pub(crate) fn params(&self) -> impl Iterator<Item = &Vec<Value>> {
        self.slots.keys()
    }

    /// Iterator over parameter tuples that received a new value this cycle.
    pub(crate) fn updated_params(&self) -> impl Iterator<Item = &Vec<Value>> {
        self.updated.iter()
    }

    /// Returns `true` if the instance for `params` was updated this cycle.
    pub(crate) fn was_updated(&self, params: &[Value]) -> bool {
        self.updated.contains(params)
    }

    /// Iterator over parameter tuples that were activated this cycle.
    pub(crate) fn added_params(&self) -> impl Iterator<Item = &Vec<Value>> {
        self.added.iter()
    }

    /// Iterator over parameter tuples that were ended this cycle.
    pub(crate) fn removed_params(&self) -> impl Iterator<Item = &Vec<Value>> {
        self.removed.iter()
    }

    /// Reset all per-cycle bookkeeping.
    pub(crate) fn next_cycle(&mut self) {
        self.updated.clear();
        self.added.clear();
        self.removed.clear();
    }

    /// Returns `true` if a live instance for `params` exists.
    pub(crate) fn has_slot(&self, params: &[Value]) -> bool {
        self.slots.contains_key(params)
    }
}

/// Central storage that holds every input signal buffer and every output
/// constraint buffer (plain or parameterized) for a running monitor.
pub(crate) struct DataStore {
    /// One buffer per input signal, indexed directly by signal index.
    inputs: Vec<ValueBuffer>,

    /// Maps a raw constraint stream index to the position inside either
    /// `plain_streams` or `param_streams`.
    pub(crate) index_table: Vec<usize>,

    /// Buffers for non-parameterized output constraints.
    plain_streams: Vec<ValueBuffer>,

    /// Buffer groups for parameterized output constraints.
    pub(crate) param_streams: Vec<SlotGroup>,

    /// Whether each raw constraint stream index is parameterized.
    parameterized_constraints: Vec<bool>,
}

impl DataStore {
    /// Build a `DataStore` from the compiled intermediate representation.
    pub(crate) fn build(ir: &OORVIR) -> DataStore {
        let (param, plain): (Vec<&ConstraintStream>, Vec<&ConstraintStream>) =
            ir.constraints.iter().partition(|c| c.is_parameter());

        let mut raw_index_table: Vec<Option<usize>> = vec![None; ir.constraints.len()];
        for (i, c) in plain.iter().enumerate() {
            raw_index_table[c.stream_idx.out_ix()] = Some(i);
        }
        for (i, c) in param.iter().enumerate() {
            raw_index_table[c.stream_idx.out_ix()] = Some(i);
        }
        debug_assert!(raw_index_table.iter().all(Option::is_some));
        let index_table = raw_index_table.into_iter().flatten().collect();

        let plain_streams = plain
            .iter()
            .map(|c| ValueBuffer::new(&c.annotation, c.storage_bound, !c.is_start()))
            .collect();

        let param_streams = param
            .iter()
            .map(|c| SlotGroup::new(&c.annotation, c.storage_bound))
            .collect();

        let inputs = ir
            .signals
            .iter()
            .map(|s| ValueBuffer::new(&s.annotation, s.storage_bound, true))
            .collect();

        let parameterized_constraints = ir.constraints.iter().map(Stream::is_parameter).collect();

        DataStore {
            inputs,
            index_table,
            plain_streams,
            param_streams,
            parameterized_constraints,
        }
    }

    /// Immutable access to the buffer for input signal `idx`.
    pub(crate) fn signal(&self, idx: usize) -> &ValueBuffer {
        &self.inputs[idx]
    }

    /// Mutable access to the buffer for input signal `idx`.
    pub(crate) fn signal_mut(&mut self, idx: usize) -> &mut ValueBuffer {
        &mut self.inputs[idx]
    }

    /// Immutable access to the buffer for a plain output constraint at stream index `idx`.
    pub(crate) fn constraint(&self, idx: usize) -> &ValueBuffer {
        &self.plain_streams[self.index_table[idx]]
    }

    /// Mutable access to the buffer for a plain output constraint at `idx`.
    pub(crate) fn constraint_mut(&mut self, idx: usize) -> &mut ValueBuffer {
        &mut self.plain_streams[self.index_table[idx]]
    }

    /// Immutable access to the `SlotGroup` for a parameterized output constraint at `idx`.
    pub(crate) fn group(&self, idx: usize) -> &SlotGroup {
        &self.param_streams[self.index_table[idx]]
    }

    /// Mutable access to the `SlotGroup` for a parameterized output constraint at `idx`.
    pub(crate) fn group_mut(&mut self, idx: usize) -> &mut SlotGroup {
        &mut self.param_streams[self.index_table[idx]]
    }

    /// Return whether the raw constraint stream at `idx` has instance parameters.
    pub(crate) fn constraint_is_parameterized(&self, idx: usize) -> bool {
        self.parameterized_constraints[idx]
    }

    /// Reset per-cycle bookkeeping for all parameterized stream groups.
    pub(crate) fn next_cycle(&mut self) {
        self.param_streams
            .iter_mut()
            .for_each(SlotGroup::next_cycle);
    }
}
