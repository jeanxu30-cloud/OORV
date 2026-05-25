use std::collections::HashMap;
use std::ops::Add;

use serde::{Deserialize, Serialize};

use crate::oorvir::source::analysis::StoragePolicy;
use crate::oorvir::source::analysis::{HasDependencies, HasMemory, StorageMap};
use crate::oorvir::source::{OORVIr1, StreamIdx};

/// Indicates how much memory is required to store a stream.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum StorageRequirement {
    /// The required memory might exceed any bound.
    Unbounded,
    /// Only the contained amount of stream entries need to be stored.
    Bounded(u32),
}

impl StorageRequirement {
    const DYNAMIC_DEFAULT_VALUE: StorageRequirement = StorageRequirement::Bounded(0);
    const STATIC_DEFAULT_VALUE: StorageRequirement = StorageRequirement::Bounded(1);

    pub fn unwrap_bound(self) -> u32 {
        if let StorageRequirement::Bounded(value) = self {
            return value;
        }
        unreachable!("Called `StorageRequirement::unwrap()` on an `Unbounded` value.")
    }

    pub fn into_option(self) -> Option<u32> {
        if let StorageRequirement::Bounded(value) = self {
            Some(value)
        } else {
            None
        }
    }

    pub(crate) fn initial_bound(kind: StoragePolicy) -> StorageRequirement {
        if matches!(kind, StoragePolicy::Static) {
            Self::STATIC_DEFAULT_VALUE
        } else {
            Self::DYNAMIC_DEFAULT_VALUE
        }
    }
}

impl Add for StorageRequirement {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (StorageRequirement::Bounded(lhs), StorageRequirement::Bounded(rhs)) => {
                StorageRequirement::Bounded(lhs + rhs)
            }
            _ => StorageRequirement::Unbounded,
        }
    }
}

impl PartialOrd for StorageRequirement {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering;
        use StorageRequirement::*;
        match (self, other) {
            (Unbounded, Unbounded) => None,
            (Bounded(_), Unbounded) => Some(Ordering::Less),
            (Unbounded, Bounded(_)) => Some(Ordering::Greater),
            (Bounded(b1), Bounded(b2)) => Some(b1.cmp(b2)),
        }
    }
}

impl HasMemory for StorageMap {
    fn required_storage_bound(&self, sr: StreamIdx) -> StorageRequirement {
        *self
            .storage_bound_per_stream
            .get(&sr)
            .expect("stream storage bound missing")
    }
}

impl StorageMap {
    pub(crate) fn determine_storage(
        spec: &OORVIr1,
        storage_bound_mode: StoragePolicy,
    ) -> StorageMap {
        let mut storage_bound_per_stream = spec
            .all_streams()
            .map(|sr| (sr, StorageRequirement::initial_bound(storage_bound_mode)))
            .collect::<HashMap<StreamIdx, StorageRequirement>>();

        for edge in spec.access_graph().edge_indices() {
            let edge_weight = spec
                .access_graph()
                .edge_weight(edge)
                .copied()
                .expect("dependency edge without weight");
            let required_bound = edge_weight.storage_cost(storage_bound_mode);
            let (_, target_node) = spec
                .access_graph()
                .edge_endpoints(edge)
                .expect("dependency edge without endpoints");
            let target_stream = *spec
                .access_graph()
                .node_weight(target_node)
                .expect("dependency node without stream index");

            let slot = storage_bound_per_stream
                .get_mut(&target_stream)
                .expect("initialized stream memory bound missing");
            if *slot < required_bound {
                *slot = required_bound;
            }
        }

        StorageMap {
            storage_bound_per_stream,
        }
    }
}
