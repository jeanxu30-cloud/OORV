use std::collections::HashMap;

use petgraph::algo::is_cyclic_directed;
use petgraph::Outgoing;
use serde::{Deserialize, Serialize};

use super::{HasSchedule, LayerAssignment};
use crate::oorvir::source::analysis::flow::FlowFilter;
use crate::oorvir::source::analysis::{FlowGraph, HasDependencies};
use crate::oorvir::source::{OORVIr1, StreamIdx};

impl HasSchedule for LayerAssignment {
    fn scheduling_layers(&self, si: StreamIdx) -> StreamLayer {
        *self
            .stream_layers
            .get(&si)
            .expect("stream layer missing in LayerAssignment")
    }
}

/// Represents a layer indicating the position when an expression can be evaluated.
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerIndex(usize);

/// Wrapper to collect the layer when a stream instance is started and evaluated.
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamLayer {
    start: LayerIndex,
    evaluation: LayerIndex,
}

impl StreamLayer {
    pub(crate) fn new(start_layer: LayerIndex, eval_layer: LayerIndex) -> StreamLayer {
        StreamLayer {
            start: start_layer,
            evaluation: eval_layer,
        }
    }

    pub fn start_layer(&self) -> LayerIndex {
        self.start
    }

    pub fn eval_layer(&self) -> LayerIndex {
        self.evaluation
    }
}

impl From<LayerIndex> for usize {
    fn from(layer: LayerIndex) -> usize {
        layer.inner()
    }
}

impl LayerIndex {
    pub fn new(layer: usize) -> Self {
        LayerIndex(layer)
    }

    pub fn inner(self) -> usize {
        self.0
    }
}

impl LayerAssignment {
    pub(crate) fn derive_order(spec: &OORVIr1) -> LayerAssignment {
        let solved_levels = Self::assign_layers(spec, spec.access_graph());
        LayerAssignment {
            stream_layers: solved_levels,
        }
    }

    fn assign_layers(spec: &OORVIr1, graph: &FlowGraph) -> HashMap<StreamIdx, StreamLayer> {
        let filtered_graph = graph
            .clone()
            .drop_backward_shifts()
            .drop_at_end()
            .drop_pacing_mismatches(spec);
        let start_only_graph = filtered_graph.clone().keep_at_start();

        debug_assert!(
            !is_cyclic_directed(&filtered_graph),
            "error: dependency graph contains cycles, cannot assign layers"
        );

        let (mut start_levels, mut evaluation_levels) = Self::init_seed_layers(spec);

        while filtered_graph.node_count() > evaluation_levels.len() {
            for node in start_only_graph.node_indices() {
                let stream_ref = *start_only_graph
                    .node_weight(node)
                    .expect("start graph node without stream index");
                if start_levels.contains_key(&stream_ref) {
                    continue;
                }
                if let Some(level) =
                    Self::compute_start_layer(node, &start_only_graph, &evaluation_levels)
                {
                    start_levels.insert(stream_ref, level);
                }
            }

            let known_start_levels = &start_levels;

            for node in filtered_graph.node_indices() {
                let stream_ref = *filtered_graph
                    .node_weight(node)
                    .expect("dependency graph node without stream index");
                if evaluation_levels.contains_key(&stream_ref)
                    || !known_start_levels.contains_key(&stream_ref)
                {
                    continue;
                }
                if let Some(mut eval_level) =
                    Self::compute_eval_layer(node, &filtered_graph, &evaluation_levels)
                {
                    let start_level = known_start_levels[&stream_ref];
                    if eval_level <= start_level {
                        eval_level = LayerIndex::new(start_level.inner() + 1);
                    }
                    evaluation_levels.insert(stream_ref, eval_level);
                }
            }
        }

        evaluation_levels
            .into_iter()
            .map(|(key, eval_layer)| {
                let layer = (key, StreamLayer::new(start_levels[&key], eval_layer));
                layer
            })
            .collect::<HashMap<StreamIdx, StreamLayer>>()
    }

    fn init_seed_layers(
        spec: &OORVIr1,
    ) -> (
        HashMap<StreamIdx, LayerIndex>,
        HashMap<StreamIdx, LayerIndex>,
    ) {
        let init = spec
            .signals()
            .map(|signal| (signal.si, LayerIndex::new(0)))
            .collect::<HashMap<StreamIdx, LayerIndex>>();

        let eval_start = 0;
        let eval = spec
            .signals()
            .map(|signal| (signal.si, LayerIndex::new(eval_start)))
            .collect::<HashMap<StreamIdx, LayerIndex>>();

        (init, eval)
    }

    fn compute_start_layer(
        node: petgraph::prelude::NodeIndex,
        start_graph: &FlowGraph,
        eval_levels: &HashMap<StreamIdx, LayerIndex>,
    ) -> Option<LayerIndex> {
        let mut max_neighbor_level = LayerIndex::new(0);
        let mut has_neighbors = false;
        for neighbor in start_graph.neighbors_directed(node, Outgoing) {
            has_neighbors = true;
            let neighbor_stream = *start_graph.node_weight(neighbor)?;
            let neighbor_level = *eval_levels.get(&neighbor_stream)?;
            max_neighbor_level = std::cmp::max(max_neighbor_level, neighbor_level);
        }

        if has_neighbors {
            Some(LayerIndex::new(max_neighbor_level.inner() + 1))
        } else {
            Some(LayerIndex::new(0))
        }
    }

    fn compute_eval_layer(
        node: petgraph::prelude::NodeIndex,
        dep_graph: &FlowGraph,
        eval_levels: &HashMap<StreamIdx, LayerIndex>,
    ) -> Option<LayerIndex> {
        let mut max_neighbor_level = LayerIndex::new(0);
        let mut has_successor = false;

        for neighbor in dep_graph.neighbors_directed(node, Outgoing) {
            if neighbor == node {
                continue;
            }
            has_successor = true;
            let neighbor_stream = *dep_graph.node_weight(neighbor)?;
            let neighbor_level = *eval_levels.get(&neighbor_stream)?;
            max_neighbor_level = std::cmp::max(max_neighbor_level, neighbor_level);
        }

        if has_successor {
            Some(LayerIndex::new(max_neighbor_level.inner() + 1))
        } else {
            Some(LayerIndex::new(1))
        }
    }
}
