pub(crate) mod ast_build;
pub(crate) mod flow;
pub(crate) mod layers;
pub(crate) mod pacing;
pub(crate) mod solver;
pub(crate) mod storage;
pub(crate) mod values;

pub use flow::{AccessSite, FlowGraph, GraphViolation, StreamEdge};
pub(crate) use flow::{NeighborIndex, ReachIndex};
pub use layers::{LayerIndex, StreamLayer};
pub use storage::StorageRequirement;

use std::collections::HashMap;

use crate::diagnostic::Diagnostic;
use crate::diagnostic::OORVError;

use crate::bind_analysis_delegate;
use crate::oorvir::source::{
    AccessMode, DataType, ExprNodeIdx, OORVIr1, StreamIdx, StreamPacingKind, StreamProfile,
};

/// A type alias representing the complete type profile of a stream.
/// Streams carry both a value type and a pacing type; `SourceType` bundles both.
pub(crate) type SourceType = StreamProfile;

/// Cached results of the type analysis pass.
#[derive(Debug, Clone)]
pub struct TypeRegistry {
    stream_types: HashMap<StreamIdx, StreamProfile>,
    expression_types: HashMap<ExprNodeIdx, StreamProfile>,
    param_types: HashMap<(StreamIdx, usize), DataType>,
    eval_types: HashMap<(StreamIdx, usize), StreamPacingKind>,
}

impl TypeRegistry {
    pub(crate) fn new(
        stream_types: HashMap<StreamIdx, StreamProfile>,
        expression_types: HashMap<ExprNodeIdx, StreamProfile>,
        param_types: HashMap<(StreamIdx, usize), DataType>,
        eval_types: HashMap<(StreamIdx, usize), StreamPacingKind>,
    ) -> Self {
        TypeRegistry {
            stream_types,
            expression_types,
            param_types,
            eval_types,
        }
    }
}

/// Storage-allocation strategy determining how memory bounds are computed.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoragePolicy {
    #[default]
    /// All values — including values that exist only within a single cycle — are counted towards memory.
    /// Every stream has a memory bound of at least 1.
    Static,
    /// Counts only values that must be retained between evaluation cycles.
    /// Streams accessed only with a synchronous (offset-0) lookup have a memory bound of 0.
    #[allow(dead_code)]
    Dynamic,
}

/// Query interface for the type analysis results.
pub trait HasTypeInfo {
    fn stream_signature(&self, sr: StreamIdx) -> SourceType;
    fn has_periodic_pacing(&self, sr: StreamIdx) -> bool;
    fn has_event_pacing(&self, sr: StreamIdx) -> bool;
    fn expression_type(&self, eid: ExprNodeIdx) -> SourceType;
    fn parameter_type_at(&self, sr: StreamIdx, idx: usize) -> DataType;
    fn eval_pacing_at(&self, sr: StreamIdx, idx: usize) -> StreamPacingKind;
}

/// Computed results of the dependency analysis pass.
#[derive(Debug, Clone)]
pub struct AccessIndex {
    pub(crate) out_neighbors: NeighborIndex,
    pub(crate) forward_reach: ReachIndex,
    pub(crate) in_neighbors: NeighborIndex,
    pub(crate) backward_reach: ReachIndex,
    pub(crate) flow: FlowGraph,
}

/// Query interface for the dependency analysis results.
pub trait HasDependencies {
    fn out_streams(&self, who: StreamIdx) -> Vec<StreamIdx>;
    fn out_streams_with_sites(
        &self,
        who: StreamIdx,
    ) -> Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)>;
    fn all_downstream(&self, who: StreamIdx) -> Vec<StreamIdx>;
    fn in_streams(&self, who: StreamIdx) -> Vec<StreamIdx>;
    fn in_streams_with_sites(
        &self,
        who: StreamIdx,
    ) -> Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)>;
    fn all_upstream(&self, who: StreamIdx) -> Vec<StreamIdx>;
    fn access_graph(&self) -> &FlowGraph;
}

/// Computed results of the evaluation-order pass.
#[derive(Debug, Clone)]
pub struct LayerAssignment {
    pub(crate) stream_layers: HashMap<StreamIdx, StreamLayer>,
}

/// Query interface for the layer/schedule analysis results.
pub trait HasSchedule {
    fn scheduling_layers(&self, sr: StreamIdx) -> StreamLayer;
}

/// Computed results of the memory-layout pass.
#[derive(Debug, Clone)]
pub struct StorageMap {
    pub(crate) storage_bound_per_stream: HashMap<StreamIdx, StorageRequirement>,
}

/// Query interface for the memory-layout analysis results.
pub trait HasMemory {
    fn required_storage_bound(&self, sr: StreamIdx) -> StorageRequirement;
}

// ── TypeRegistry implementation ────────────────────────────────────────────

impl HasTypeInfo for TypeRegistry {
    fn stream_signature(&self, sr: StreamIdx) -> SourceType {
        self.fetch_stream_type(sr)
    }

    fn has_periodic_pacing(&self, sr: StreamIdx) -> bool {
        self.fetch_stream_type(sr).execution_rate.is_clock_rate()
    }

    fn has_event_pacing(&self, sr: StreamIdx) -> bool {
        self.fetch_stream_type(sr).execution_rate.is_conditional()
    }

    fn expression_type(&self, eid: ExprNodeIdx) -> SourceType {
        self.fetch_expr_type(eid)
    }

    fn parameter_type_at(&self, sr: StreamIdx, idx: usize) -> DataType {
        self.fetch_param_type(sr, idx)
    }

    fn eval_pacing_at(&self, sr: StreamIdx, idx: usize) -> StreamPacingKind {
        self.fetch_eval_pacing(sr, idx)
    }
}

impl TypeRegistry {
    pub fn fetch_stream_type(&self, stream: StreamIdx) -> StreamProfile {
        self.stream_types
            .get(&stream)
            .cloned()
            .expect("stream type missing in TypeRegistry")
    }

    pub fn fetch_expr_type(&self, expression: ExprNodeIdx) -> StreamProfile {
        self.expression_types
            .get(&expression)
            .cloned()
            .expect("expression type missing in TypeRegistry")
    }

    pub fn fetch_param_type(&self, stream: StreamIdx, index: usize) -> DataType {
        self.param_types
            .get(&(stream, index))
            .cloned()
            .expect("parameter type missing in TypeRegistry")
    }

    pub fn fetch_eval_pacing(&self, stream: StreamIdx, eval_index: usize) -> StreamPacingKind {
        self.eval_types
            .get(&(stream, eval_index))
            .cloned()
            .expect("eval pacing type missing in TypeRegistry")
    }
}

// ── OORVIr1 analysis pass runners ──────────────────────────────────────────

impl OORVIr1 {
    pub fn run_type_pass(mut self) -> Result<OORVIr1, OORVError> {
        let tts = crate::oorvir::source::analysis::solver::perform_analysis(&self)?;
        self.types = Some(tts);
        Ok(self)
    }

    pub fn run_dep_pass(mut self) -> Result<OORVIr1, OORVError> {
        let dependencies = AccessIndex::collect_edges(&self)?;
        self.dependencies = Some(dependencies);
        Ok(self)
    }

    pub fn run_layer_pass(mut self) -> Result<OORVIr1, OORVError> {
        let order = LayerAssignment::derive_order(&self);
        self.layers = Some(order);
        Ok(self)
    }

    pub fn run_memory_pass(mut self) -> Result<OORVIr1, OORVError> {
        let memory = StorageMap::determine_storage(&self, StoragePolicy::Static);
        self.memory = Some(memory);
        Ok(self)
    }

    pub fn finalize_ir(self) -> Result<OORVIr1, OORVError> {
        if self.types.is_none()
            || self.dependencies.is_none()
            || self.layers.is_none()
            || self.memory.is_none()
        {
            return Err(OORVError::from(Diagnostic::error(
                "cannot seal IR: one or more analysis passes have not been completed",
            )));
        }
        Ok(self)
    }
}

// ── Delegation blocks: forward each analysis trait to the stored pass result ─

bind_analysis_delegate! {
    impl HasTypeInfo for OORVIr1 via types("type analysis pass has not been applied") {
        fn stream_signature(&self, sr: StreamIdx) -> SourceType;
        fn has_periodic_pacing(&self, sr: StreamIdx) -> bool;
        fn has_event_pacing(&self, sr: StreamIdx) -> bool;
        fn expression_type(&self, eid: ExprNodeIdx) -> SourceType;
        fn parameter_type_at(&self, sr: StreamIdx, idx: usize) -> DataType;
        fn eval_pacing_at(&self, sr: StreamIdx, idx: usize) -> StreamPacingKind;
    }
}

bind_analysis_delegate! {
    impl HasDependencies for OORVIr1 via dependencies("dependency analysis pass has not been applied") {
        fn out_streams(&self, who: StreamIdx) -> Vec<StreamIdx>;
        fn out_streams_with_sites(&self, who: StreamIdx) -> Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)>;
        fn all_downstream(&self, who: StreamIdx) -> Vec<StreamIdx>;
        fn in_streams(&self, who: StreamIdx) -> Vec<StreamIdx>;
        fn in_streams_with_sites(&self, who: StreamIdx) -> Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)>;
        fn all_upstream(&self, who: StreamIdx) -> Vec<StreamIdx>;
        fn access_graph(&self) -> &FlowGraph;
    }
}

bind_analysis_delegate! {
    impl HasSchedule for OORVIr1 via layers("evaluation schedule pass has not been applied") {
        fn scheduling_layers(&self, sr: StreamIdx) -> StreamLayer;
    }
}

bind_analysis_delegate! {
    impl HasMemory for OORVIr1 via memory("memory layout pass has not been applied") {
        fn required_storage_bound(&self, sr: StreamIdx) -> StorageRequirement;
    }
}
