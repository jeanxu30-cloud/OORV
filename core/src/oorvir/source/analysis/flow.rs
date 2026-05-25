use std::collections::{HashMap, HashSet};

use crate::ast::SourceSpan;
use crate::diagnostic::{Diagnostic, OORVError};
use itertools::Itertools;
use petgraph::algo::{all_simple_paths, has_path_connecting};
use petgraph::graph::NodeIndex;
use petgraph::stable_graph::StableGraph;
use petgraph::visit::{IntoNeighbors, IntoNodeIdentifiers, Visitable};
use petgraph::Outgoing;
use serde::{Deserialize, Serialize};

use super::{AccessIndex, HasDependencies, HasTypeInfo};
use crate::oorvir::source::analysis::StoragePolicy;
use crate::oorvir::source::{
    AccessMode, ExprVariant, Expression, FnExprKind, InitView, OORVIr1, StorageRequirement,
    StreamIdx, StreamPacingKind, WidenExprKind,
};

/// A directed graph where each node holds a [`StreamIdx`] and each edge
/// represents a data dependency with an associated [`StreamEdge`] weight.
pub type FlowGraph = StableGraph<StreamIdx, StreamEdge>;

/// Metadata attached to every edge in the [`FlowGraph`].
#[derive(Hash, Clone, Debug, PartialEq, Eq, Copy)]
pub struct StreamEdge {
    /// Which clause of the source stream produces this dependency.
    pub site: AccessSite,
    /// How the source stream accesses the target stream.
    pub access: AccessMode,
}

/// Identifies which syntactic clause of a stream produces a given dependency.
#[derive(Hash, Clone, Debug, PartialEq, Eq, Copy, Serialize, Deserialize)]
pub enum AccessSite {
    /// The dependency arises in the `start` clause.
    AtStart,
    /// The dependency arises in the `eval when` condition of eval slot `n`.
    AtCondition(usize),
    /// The dependency arises in the `eval` expression of eval slot `n`.
    AtBody(usize),
    /// The dependency arises in the `end` clause.
    AtEnd,
}

impl StreamEdge {
    pub(crate) fn new(access: AccessMode, site: AccessSite) -> Self {
        Self { access, site }
    }

    pub(crate) fn storage_cost(&self, policy: StoragePolicy) -> StorageRequirement {
        match self.access {
            AccessMode::Strict | AccessMode::Get | AccessMode::Fresh => {
                StorageRequirement::initial_bound(policy)
            }
            AccessMode::Cached => StorageRequirement::Bounded(1),
            AccessMode::Shift(o) => o.as_storage_bound(),
        }
    }
}

/// Maps each stream to its directly reachable neighbours together with per-edge metadata.
pub(crate) type NeighborIndex = HashMap<StreamIdx, Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)>>;

/// Maps each stream to the set of streams reachable transitively from it.
pub(crate) type ReachIndex = HashMap<StreamIdx, Vec<StreamIdx>>;

/// Extension methods on [`FlowGraph`] for constructing filtered views.
pub(crate) trait FlowFilter {
    fn drop_backward_shifts(self) -> Self;
    fn drop_pacing_mismatches(self, ir: &OORVIr1) -> Self;
    fn is_backward_shift(e: &StreamEdge) -> bool {
        match e.access {
            AccessMode::Strict | AccessMode::Get | AccessMode::Fresh | AccessMode::Cached => false,
            AccessMode::Shift(o) => o.is_backward_shift(),
        }
    }
    fn drop_at_end(self) -> Self;
    fn keep_at_start(self) -> Self;
}

impl FlowFilter for FlowGraph {
    fn drop_backward_shifts(mut self) -> Self {
        self.retain_edges(|g, ei| !Self::is_backward_shift(g.edge_weight(ei).unwrap()));
        self
    }

    fn drop_pacing_mismatches(mut self, ir: &OORVIr1) -> Self {
        self.retain_edges(|g, ei| {
            let (lhs_node, rhs_node) = g.edge_endpoints(ei).unwrap();
            let edge = g.edge_weight(ei).unwrap();
            let lhs_sig = ir.stream_signature(*g.node_weight(lhs_node).unwrap());
            let rhs_pacing = ir
                .stream_signature(*g.node_weight(rhs_node).unwrap())
                .execution_rate;
            let lhs_pacing = match edge.site {
                AccessSite::AtStart => lhs_sig.init_rate,
                AccessSite::AtCondition(_) | AccessSite::AtBody(_) => lhs_sig.execution_rate,
                AccessSite::AtEnd => lhs_sig.termination_rate,
            };
            match (lhs_pacing, rhs_pacing) {
                (StreamPacingKind::Conditional(_), StreamPacingKind::Conditional(_)) => true,
                (StreamPacingKind::Conditional(_), _) | (_, StreamPacingKind::Conditional(_)) => {
                    false
                }
                (StreamPacingKind::GlobalClock(_), StreamPacingKind::GlobalClock(_)) => true,
                (StreamPacingKind::LocalClock(_), StreamPacingKind::LocalClock(_)) => true,
                (StreamPacingKind::LocalClock(_), StreamPacingKind::GlobalClock(_))
                | (StreamPacingKind::GlobalClock(_), StreamPacingKind::LocalClock(_)) => true,
                _ => unreachable!(),
            }
        });
        self
    }

    fn drop_at_end(mut self) -> Self {
        self.retain_edges(|g, ei| g.edge_weight(ei).unwrap().site != AccessSite::AtEnd);
        self
    }

    fn keep_at_start(mut self) -> Self {
        self.retain_edges(|g, ei| g.edge_weight(ei).unwrap().site == AccessSite::AtStart);
        self
    }
}

impl HasDependencies for AccessIndex {
    fn out_streams(&self, who: StreamIdx) -> Vec<StreamIdx> {
        self.out_streams_with_sites(who)
            .into_iter()
            .map(|(dst, _)| dst)
            .collect()
    }

    fn out_streams_with_sites(
        &self,
        who: StreamIdx,
    ) -> Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)> {
        self.out_neighbors.get(&who).cloned().unwrap_or_default()
    }

    fn all_downstream(&self, who: StreamIdx) -> Vec<StreamIdx> {
        self.forward_reach.get(&who).cloned().unwrap_or_default()
    }

    fn in_streams(&self, who: StreamIdx) -> Vec<StreamIdx> {
        self.in_streams_with_sites(who)
            .into_iter()
            .map(|(src, _)| src)
            .collect()
    }

    fn in_streams_with_sites(
        &self,
        who: StreamIdx,
    ) -> Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)> {
        self.in_neighbors.get(&who).cloned().unwrap_or_default()
    }

    fn all_upstream(&self, who: StreamIdx) -> Vec<StreamIdx> {
        self.backward_reach.get(&who).cloned().unwrap_or_default()
    }

    fn access_graph(&self) -> &FlowGraph {
        &self.flow
    }
}

/// Errors that the dependency analysis can produce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GraphViolation {
    /// The specification contains a non-negative dependency cycle.
    CyclicFlow(Vec<StreamIdx>),
}

impl GraphViolation {
    pub(crate) fn into_diagnostic(self, ir: &OORVIr1) -> Diagnostic {
        let labels = ir.names();
        let spans: HashMap<StreamIdx, SourceSpan> = ir
            .signals()
            .map(|s| (s.si, s.span))
            .chain(ir.constraints().map(|c| (c.si, c.span)))
            .collect();
        match self {
            GraphViolation::CyclicFlow(mut cycle) => {
                if cycle.len() == 1
                    || cycle[0] != *cycle.last().expect("cycle has at least one stream")
                {
                    cycle.push(cycle[0]);
                }
                let path_str = cycle.iter().map(|si| &labels[si]).join(" -> ");
                let mut diag = Diagnostic::error(&format!(
                    "ill-formed specification: dependency cycle detected: {path_str}"
                ));
                for sid in cycle.iter().take(cycle.len() - 1) {
                    diag = diag.add_span_with_label(
                        spans[sid],
                        Some(&format!("stream `{}` is part of the cycle", labels[sid])),
                        true,
                    );
                }
                diag
            }
        }
    }
}

impl AccessIndex {
    pub(crate) fn collect_edges(ir: &OORVIr1) -> Result<AccessIndex, OORVError> {
        let raw_links = Self::all_links(ir);
        let net = Self::build_flow(ir, &raw_links);

        Self::assert_acyclic(&net, ir).map_err(|violation| violation.into_diagnostic(ir))?;

        let (fwd_raw, rev_raw) = Self::split_direct_links(ir, &raw_links);
        let out_neighbors = Self::bucket_edges(fwd_raw);
        let in_neighbors = Self::bucket_edges(rev_raw);
        let forward_reach = Self::reach_from(&net, true);
        let backward_reach = Self::reach_from(&net, false);

        Ok(AccessIndex {
            out_neighbors,
            forward_reach,
            in_neighbors,
            backward_reach,
            flow: net,
        })
    }

    #[allow(clippy::type_complexity)]
    fn bucket_edges(
        raw: HashMap<StreamIdx, Vec<(StreamIdx, AccessSite, AccessMode)>>,
    ) -> HashMap<StreamIdx, Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)>> {
        let mut result = HashMap::new();
        for (node, pairs) in raw {
            let mut grouped: HashMap<StreamIdx, Vec<(AccessSite, AccessMode)>> = HashMap::new();
            for (dst, site, mode) in pairs {
                grouped.entry(dst).or_default().push((site, mode));
            }
            let mut sorted: Vec<_> = grouped.into_iter().collect();
            sorted.sort_by_key(|(dst, _)| *dst);
            result.insert(node, sorted);
        }
        result
    }

    fn are_reachable(net: &FlowGraph, src: NodeIndex, dst: NodeIndex) -> bool {
        if src != dst {
            has_path_connecting(net, src, dst, None)
        } else {
            let self_loop = net.edge_indices().find(|ei| {
                let (s, t) = net.edge_endpoints(*ei).unwrap();
                s == t && s == src
            });
            let via_neighbor = net
                .neighbors_directed(src, Outgoing)
                .any(|nbr| has_path_connecting(net, nbr, dst, None));
            self_loop.is_some() || via_neighbor
        }
    }

    fn find_back_edge<G>(graph: G) -> Result<(), (G::NodeId, G::NodeId)>
    where
        G: IntoNodeIdentifiers + IntoNeighbors + Visitable,
    {
        use petgraph::visit::{depth_first_search, DfsEvent};
        depth_first_search(graph, graph.node_identifiers(), |ev| match ev {
            DfsEvent::BackEdge(tail, head) => Err((tail, head)),
            _ => Ok(()),
        })
    }

    fn assert_acyclic(net: &FlowGraph, ir: &OORVIr1) -> Result<(), GraphViolation> {
        let filtered: &FlowGraph = &net
            .clone()
            .drop_pacing_mismatches(ir)
            .drop_backward_shifts()
            .drop_at_end();
        Self::find_back_edge(filtered).map_err(|(tail, head)| {
            let cycle_path: Vec<NodeIndex> = all_simple_paths(filtered, head, tail, 0, None)
                .next()
                .expect("back-edge implies a simple path between its endpoints");
            let cycle_nodes: Vec<StreamIdx> = cycle_path.iter().map(|id| filtered[*id]).collect();
            GraphViolation::CyclicFlow(cycle_nodes)
        })
    }

    fn links_in_expr(
        _ir: &OORVIr1,
        origin_stream: StreamIdx,
        node: &Expression,
    ) -> Vec<(StreamIdx, AccessMode, StreamIdx)> {
        match &node.kind {
            ExprVariant::StreamAccess(dst, ak, params) => {
                let mut result = params
                    .iter()
                    .flat_map(|p| Self::links_in_expr(_ir, origin_stream, p))
                    .collect::<Vec<_>>();
                result.push((origin_stream, *ak, *dst));
                result
            }
            ExprVariant::ParameterAccess(_, _)
            | ExprVariant::LoadConstant(_)
            | ExprVariant::QuantifiedVar(_) => Vec::new(),
            ExprVariant::ArithLog(_, args) => args
                .iter()
                .flat_map(|a| Self::links_in_expr(_ir, origin_stream, a))
                .collect(),
            ExprVariant::Tuple(elems) => elems
                .iter()
                .flat_map(|e| Self::links_in_expr(_ir, origin_stream, e))
                .collect(),
            ExprVariant::Function(FnExprKind { args, .. }) => args
                .iter()
                .flat_map(|a| Self::links_in_expr(_ir, origin_stream, a))
                .collect(),
            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => Self::links_in_expr(_ir, origin_stream, condition)
                .into_iter()
                .chain(Self::links_in_expr(_ir, origin_stream, consequence))
                .chain(Self::links_in_expr(_ir, origin_stream, alternative))
                .collect(),
            ExprVariant::TupleAccess(inner, _) => Self::links_in_expr(_ir, origin_stream, inner),
            ExprVariant::Widen(WidenExprKind { expr: inner, .. }) => {
                Self::links_in_expr(_ir, origin_stream, inner)
            }
            ExprVariant::Default { expr, default } => Self::links_in_expr(_ir, origin_stream, expr)
                .into_iter()
                .chain(Self::links_in_expr(_ir, origin_stream, default))
                .collect(),
            ExprVariant::Quantified(_, _, _, inner) => {
                Self::links_in_expr(_ir, origin_stream, inner)
            }
            _ => unreachable!("all expression variants must be handled"),
        }
    }

    fn all_links(ir: &OORVIr1) -> Vec<(StreamIdx, StreamEdge, StreamIdx)> {
        let mut links = Vec::new();
        for out_stream in ir.constraints().map(|c| c.si) {
            links.extend(Self::quantified_domain_links(ir, out_stream));
            for (ei, ev) in ir.eval_unchecked(out_stream).iter().enumerate() {
                for (s, ak, t) in Self::links_in_expr(ir, out_stream, ev.expression) {
                    links.push((s, StreamEdge::new(ak, AccessSite::AtBody(ei)), t));
                }
                if let Some(cond) = ev.condition {
                    for (s, ak, t) in Self::links_in_expr(ir, out_stream, cond) {
                        links.push((s, StreamEdge::new(ak, AccessSite::AtCondition(ei)), t));
                    }
                }
            }
            if let Some(InitView {
                expression,
                condition,
                ..
            }) = ir.start(out_stream)
            {
                if let Some(start_expr) = expression {
                    for (s, ak, t) in Self::links_in_expr(ir, out_stream, start_expr) {
                        links.push((s, StreamEdge::new(ak, AccessSite::AtStart), t));
                    }
                }
                if let Some(start_cond) = condition {
                    for (s, ak, t) in Self::links_in_expr(ir, out_stream, start_cond) {
                        links.push((s, StreamEdge::new(ak, AccessSite::AtStart), t));
                    }
                }
            }
            if let Some(end_expr) = ir.end(out_stream).and_then(|e| e.condition) {
                for (s, ak, t) in Self::links_in_expr(ir, out_stream, end_expr) {
                    links.push((s, StreamEdge::new(ak, AccessSite::AtEnd), t));
                }
            }
        }
        links
    }

    fn quantified_domain_links(
        ir: &OORVIr1,
        origin_stream: StreamIdx,
    ) -> Vec<(StreamIdx, StreamEdge, StreamIdx)> {
        let mut links = Vec::new();
        for domain in Self::quantified_domains_in_constraint(ir, origin_stream) {
            if let Some(target) = Self::domain_stream(ir, &domain) {
                links.push((
                    origin_stream,
                    StreamEdge::new(AccessMode::Cached, AccessSite::AtCondition(0)),
                    target,
                ));
            }
        }
        links
    }

    fn quantified_domains_in_constraint(ir: &OORVIr1, stream: StreamIdx) -> Vec<String> {
        let mut domains = HashSet::new();
        for ev in ir.eval_unchecked(stream) {
            if let Some(cond) = ev.condition {
                Self::collect_quantified_domains(cond, &mut domains);
            }
            Self::collect_quantified_domains(ev.expression, &mut domains);
        }
        let mut sorted: Vec<String> = domains.into_iter().collect();
        sorted.sort();
        sorted
    }

    fn collect_quantified_domains(expr: &Expression, domains: &mut HashSet<String>) {
        match &expr.kind {
            ExprVariant::Quantified(_, _, domain_idents, inner) => {
                for domain in domain_idents {
                    domains.insert(domain.name.clone());
                }
                Self::collect_quantified_domains(inner, domains);
            }
            ExprVariant::ArithLog(_, args) | ExprVariant::Tuple(args) => {
                for arg in args {
                    Self::collect_quantified_domains(arg, domains);
                }
            }
            ExprVariant::Function(FnExprKind { args, .. }) => {
                for arg in args {
                    Self::collect_quantified_domains(arg, domains);
                }
            }
            ExprVariant::StreamAccess(_, _, params) => {
                for param in params {
                    Self::collect_quantified_domains(param, domains);
                }
            }
            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => {
                Self::collect_quantified_domains(condition, domains);
                Self::collect_quantified_domains(consequence, domains);
                Self::collect_quantified_domains(alternative, domains);
            }
            ExprVariant::TupleAccess(inner, _) => Self::collect_quantified_domains(inner, domains),
            ExprVariant::Widen(WidenExprKind { expr, .. }) => {
                Self::collect_quantified_domains(expr, domains);
            }
            ExprVariant::Default { expr, default } => {
                Self::collect_quantified_domains(expr, domains);
                Self::collect_quantified_domains(default, domains);
            }
            ExprVariant::ParameterAccess(_, _)
            | ExprVariant::FunctionParameterAccess(_, _, _)
            | ExprVariant::QuantifiedVar(_)
            | ExprVariant::LoadConstant(_) => {}
        }
    }

    fn domain_stream(ir: &OORVIr1, domain: &str) -> Option<StreamIdx> {
        let class_name = ir
            .object_domains()
            .find_map(|(candidate, class_name)| (candidate == domain).then_some(class_name))?;
        let class_suffix = class_name.rsplit("::").next().unwrap_or(class_name);
        ir.constraints()
            .find(|constraint| {
                let name = constraint.name();
                let stream_class = name
                    .strip_suffix("_params")
                    .unwrap_or(&name)
                    .rsplit_once("::")
                    .map(|(prefix, _)| prefix)
                    .unwrap_or("");
                let stream_suffix = stream_class.rsplit("::").next().unwrap_or(stream_class);
                constraint.params().next().is_some()
                    && name.ends_with("_params")
                    && (stream_class == class_name || stream_suffix == class_suffix)
            })
            .map(|constraint| constraint.si)
    }

    fn build_flow(ir: &OORVIr1, links: &[(StreamIdx, StreamEdge, StreamIdx)]) -> FlowGraph {
        let mut net = StableGraph::with_capacity(ir.num_inputs() + ir.num_outputs(), links.len());
        let mut node_ids: HashMap<StreamIdx, NodeIndex> = HashMap::new();
        for sid in ir.all_streams() {
            node_ids.insert(sid, net.add_node(sid));
        }
        for (s, e, t) in links {
            net.add_edge(node_ids[s], node_ids[t], *e);
        }
        net
    }

    fn split_direct_links(
        ir: &OORVIr1,
        links: &[(StreamIdx, StreamEdge, StreamIdx)],
    ) -> (
        HashMap<StreamIdx, Vec<(StreamIdx, AccessSite, AccessMode)>>,
        HashMap<StreamIdx, Vec<(StreamIdx, AccessSite, AccessMode)>>,
    ) {
        let mut fwd: HashMap<StreamIdx, Vec<(StreamIdx, AccessSite, AccessMode)>> =
            ir.all_streams().map(|s| (s, Vec::new())).collect();
        let mut rev: HashMap<StreamIdx, Vec<(StreamIdx, AccessSite, AccessMode)>> =
            ir.all_streams().map(|s| (s, Vec::new())).collect();
        let mut seen_fwd: HashMap<StreamIdx, HashSet<(StreamIdx, AccessSite, AccessMode)>> =
            ir.all_streams().map(|s| (s, HashSet::new())).collect();
        let mut seen_rev: HashMap<StreamIdx, HashSet<(StreamIdx, AccessSite, AccessMode)>> =
            ir.all_streams().map(|s| (s, HashSet::new())).collect();

        for (s, e, t) in links {
            let fwd_entry = (*t, e.site, e.access);
            if seen_fwd
                .get_mut(s)
                .expect("fwd dedup set missing")
                .insert(fwd_entry)
            {
                fwd.get_mut(s).expect("fwd map missing").push(fwd_entry);
            }
            let rev_entry = (*s, e.site, e.access);
            if seen_rev
                .get_mut(t)
                .expect("rev dedup set missing")
                .insert(rev_entry)
            {
                rev.get_mut(t).expect("rev map missing").push(rev_entry);
            }
        }
        (fwd, rev)
    }

    fn reach_from(net: &FlowGraph, outward: bool) -> HashMap<StreamIdx, Vec<StreamIdx>> {
        let mut table = HashMap::new();
        for n in net.node_indices() {
            let sid = *net.node_weight(n).expect("node without stream id");
            let reach: Vec<StreamIdx> = net
                .node_indices()
                .filter(|&peer| {
                    let linked = if outward {
                        Self::are_reachable(net, n, peer)
                    } else {
                        Self::are_reachable(net, peer, n)
                    };
                    linked
                })
                .map(|peer| *net.node_weight(peer).expect("node without stream id"))
                .collect();
            table.insert(sid, reach);
        }
        table
    }
}
