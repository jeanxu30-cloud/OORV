use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Debug;

use crate::ast::SourceSpan;
use crate::diagnostic::{Diagnostic, OORVError};
use rusttyc::TcKey;

use crate::oorvir::source::analysis::pacing::PacingAnalyzer;
use crate::oorvir::source::analysis::values::ValueAnalyzer;
use crate::oorvir::source::analysis::TypeRegistry;
use crate::oorvir::source::{
    Constant, ExprNodeIdx, ExprVariant, Expression, Literal, OORVIr1, StreamIdx,
};
use crate::oorvir::source::{DataType, StreamPacingBundle, StreamPacingKind, StreamProfile};

/// Runs the two-phase (pacing + value) type analysis over the given IR and
/// returns the resulting registry of resolved type annotations.
pub(crate) fn perform_analysis(spec: &OORVIr1) -> Result<TypeRegistry, OORVError> {
    let mut engine = TypeSolver::new(spec);
    engine.run()
}

/// Trait implemented by each error variant to convert itself into a user-facing diagnostic.
pub(crate) trait FaultReporter: Debug {
    fn into_diagnostic(
        self,
        spans: &[&HashMap<TcKey, SourceSpan>],
        names: &HashMap<StreamIdx, String>,
        key1: Option<TcKey>,
        key2: Option<TcKey>,
    ) -> Diagnostic;
}

/// Tags any node that can be given a type during the inference passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeRef {
    /// An input or output stream node.
    StreamIdx(StreamIdx),
    /// The i-th eval clause belonging to a stream.
    Eval(usize, StreamIdx),
    /// A single expression node in the IR expression pool.
    Expr(ExprNodeIdx),
    /// The i-th formal parameter of a stream.
    Param(usize, StreamIdx),
}

/// Wraps an error value together with up to two RustTyc keys for diagnostics.
#[derive(Clone, Debug)]
pub(crate) struct CheckFailure<K: FaultReporter> {
    pub(crate) kind: K,
    pub(crate) key1: Option<TcKey>,
    pub(crate) key2: Option<TcKey>,
}

impl<E: FaultReporter> From<E> for CheckFailure<E> {
    fn from(e: E) -> Self {
        CheckFailure {
            kind: e,
            key1: None,
            key2: None,
        }
    }
}

impl<K: FaultReporter> CheckFailure<K> {
    pub(crate) fn into_diagnostic(
        self,
        spans: &[&HashMap<TcKey, SourceSpan>],
        names: &HashMap<StreamIdx, String>,
    ) -> Diagnostic {
        self.kind
            .into_diagnostic(spans, names, self.key1, self.key2)
    }
}

/// Drives the full two-phase type analysis (pacing then values) over an IR specification.
#[derive(Clone, Debug)]
pub struct TypeSolver<'a> {
    pub(crate) spec: &'a OORVIr1,
    pub(crate) names: HashMap<StreamIdx, String>,
}

impl<'a> TypeSolver<'a> {
    pub(crate) fn new(spec: &'a OORVIr1) -> Self {
        let stream_names = spec.names();
        TypeSolver {
            spec,
            names: stream_names,
        }
    }

    pub(crate) fn run(&mut self) -> Result<TypeRegistry, OORVError> {
        let rate_map = self.infer_pacing()?;
        let value_map = self.infer_values()?;

        let (streams, expressions, params) = self.assemble_node_types(&rate_map, &value_map);
        let eval_rates = self.gather_eval_pacing(&rate_map);

        Ok(TypeRegistry::new(streams, expressions, params, eval_rates))
    }

    fn assemble_node_types(
        &self,
        rate_map: &HashMap<NodeRef, StreamPacingBundle>,
        value_map: &HashMap<NodeRef, DataType>,
    ) -> (
        HashMap<StreamIdx, StreamProfile>,
        HashMap<ExprNodeIdx, StreamProfile>,
        HashMap<(StreamIdx, usize), DataType>,
    ) {
        let placeholder = Expression {
            kind: ExprVariant::LoadConstant(Constant::Basic(Literal::Bool(true))),
            eid: ExprNodeIdx(u32::MAX),
            span: SourceSpan::Unknown,
        };

        let mut streams: HashMap<StreamIdx, StreamProfile> = HashMap::new();
        let mut expressions: HashMap<ExprNodeIdx, StreamProfile> = HashMap::new();
        let mut params: HashMap<(StreamIdx, usize), DataType> = HashMap::new();

        for (node, vty) in value_map {
            let profile = if let Some(rp) = rate_map.get(node) {
                StreamProfile {
                    data_kind: vty.clone(),
                    execution_rate: rp.execution_rate.clone(),
                    execution_guard: rp.execution_guard.clone(),
                    init_rate: rp.init_rate.clone(),
                    init_guard: rp.init_guard.clone(),
                    termination_rate: rp.termination_rate.clone(),
                    termination_guard: rp.termination_guard.clone(),
                }
            } else {
                StreamProfile {
                    data_kind: vty.clone(),
                    execution_rate: StreamPacingKind::UnknownClock,
                    execution_guard: placeholder.clone(),
                    init_rate: StreamPacingKind::UnknownClock,
                    init_guard: placeholder.clone(),
                    termination_rate: StreamPacingKind::UnknownClock,
                    termination_guard: placeholder.clone(),
                }
            };

            match node {
                NodeRef::StreamIdx(sr) => {
                    streams.insert(*sr, profile);
                }
                NodeRef::Expr(eid) => {
                    expressions.insert(*eid, profile);
                }
                NodeRef::Param(idx, sr) => {
                    params.insert((*sr, *idx), profile.data_kind);
                }
                NodeRef::Eval(_, _) => unreachable!("eval nodes carry no value type"),
            }
        }

        (streams, expressions, params)
    }

    fn gather_eval_pacing(
        &self,
        rate_map: &HashMap<NodeRef, StreamPacingBundle>,
    ) -> HashMap<(StreamIdx, usize), StreamPacingKind> {
        rate_map
            .iter()
            .filter_map(|(node, bundle)| match node {
                NodeRef::Eval(clause_idx, stream_ref) => {
                    Some(((*stream_ref, *clause_idx), bundle.execution_rate.clone()))
                }
                _ => None,
            })
            .collect()
    }

    pub(crate) fn infer_pacing(
        &mut self,
    ) -> Result<HashMap<NodeRef, StreamPacingBundle>, OORVError> {
        PacingAnalyzer::new(self.spec, &self.names).run()
    }

    pub(crate) fn infer_values(&self) -> Result<HashMap<NodeRef, DataType>, OORVError> {
        ValueAnalyzer::new(self.spec, &self.names).analyze()
    }
}

impl PartialOrd for NodeRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (NodeRef::Expr(a), NodeRef::Expr(b)) => Some(a.cmp(b)),
            (NodeRef::StreamIdx(a), NodeRef::StreamIdx(b)) => Some(a.cmp(b)),
            (NodeRef::Param(_, _), _) => unreachable!(),
            _ => None,
        }
    }
}
