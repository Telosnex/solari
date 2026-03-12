/*
 * Sparse weight-only calculator for contraction hierarchy queries.
 *
 * Unlike PathCalculator/WeightCalculator which allocate dense arrays over all
 * graph nodes (~19 GB per instance for a 479M node graph), this uses HashMaps
 * that only track visited nodes. A typical CH query visits ~1K-10K nodes, so
 * each instance costs ~1 MB instead of ~19 GB, enabling true parallelism.
 */

use std::collections::HashMap;
use std::collections::BinaryHeap;

use crate::constants::{NodeId, Weight, WEIGHT_MAX};
use crate::fast_graph::FastGraph;
use crate::heap_item::HeapItem;

struct NodeState {
    weight: Weight,
    settled: bool,
}

pub struct SparseWeightCalculator {
    num_nodes: usize,
    fwd: HashMap<NodeId, NodeState>,
    bwd: HashMap<NodeId, NodeState>,
    heap_fwd: BinaryHeap<HeapItem>,
    heap_bwd: BinaryHeap<HeapItem>,
}

impl SparseWeightCalculator {
    pub fn new(num_nodes: usize) -> Self {
        SparseWeightCalculator {
            num_nodes,
            fwd: HashMap::with_capacity(4096),
            bwd: HashMap::with_capacity(4096),
            heap_fwd: BinaryHeap::with_capacity(4096),
            heap_bwd: BinaryHeap::with_capacity(4096),
        }
    }

    pub fn calc_weight_multiple_sources_and_targets<G: FastGraph>(
        &mut self,
        graph: &G,
        starts: Vec<(NodeId, Weight)>,
        ends: Vec<(NodeId, Weight)>,
    ) -> Option<Weight> {
        assert_eq!(graph.get_num_nodes(), self.num_nodes, "given graph has invalid node count");
        assert!(!starts.is_empty(), "there has to be at least one start");
        assert!(!ends.is_empty(), "there has to be at least one end");

        // Clear from previous query, but keep allocated capacity
        self.fwd.clear();
        self.bwd.clear();
        self.heap_fwd.clear();
        self.heap_bwd.clear();

        let mut best_weight = WEIGHT_MAX;

        // Check direct source==target matches
        for (start_node, start_weight) in &starts {
            for (end_node, end_weight) in &ends {
                if *start_node == *end_node
                    && *start_weight < WEIGHT_MAX
                    && *end_weight < WEIGHT_MAX
                    && *start_weight + *end_weight < best_weight
                {
                    best_weight = *start_weight + *end_weight;
                }
            }
        }

        // Initialize forward search
        for (node, weight) in starts {
            if weight < self.get_weight_fwd(node) {
                self.set_fwd(node, weight, false);
                self.heap_fwd.push(HeapItem::new(weight, node));
            }
        }

        // Initialize backward search
        for (node, weight) in ends {
            if weight < self.get_weight_bwd(node) {
                self.set_bwd(node, weight, false);
                self.heap_bwd.push(HeapItem::new(weight, node));
            }
        }

        // Alternating bidirectional search
        loop {
            if self.heap_fwd.is_empty() && self.heap_bwd.is_empty() {
                break;
            }

            // Forward step
            loop {
                if self.heap_fwd.is_empty() {
                    break;
                }
                let curr = self.heap_fwd.pop().unwrap();
                if self.is_settled_fwd(curr.node_id) {
                    continue;
                }
                if curr.weight > best_weight {
                    break;
                }
                if self.is_stallable_fwd(graph, curr) {
                    continue;
                }
                let begin = graph.begin_out_edges(curr.node_id);
                let end = graph.end_out_edges(curr.node_id);
                for edge_id in begin..end {
                    let adj = graph.edges_fwd()[edge_id].adj_node;
                    let edge_weight = graph.edges_fwd()[edge_id].weight;
                    let weight = curr.weight + edge_weight;
                    if weight < self.get_weight_fwd(adj) {
                        self.set_fwd(adj, weight, false);
                        self.heap_fwd.push(HeapItem::new(weight, adj));
                    }
                }
                self.settle_fwd(curr.node_id);
                let bwd_weight = self.get_weight_bwd(curr.node_id);
                if bwd_weight < WEIGHT_MAX && curr.weight + bwd_weight < best_weight {
                    best_weight = curr.weight + bwd_weight;
                }
                break;
            }

            // Backward step
            loop {
                if self.heap_bwd.is_empty() {
                    break;
                }
                let curr = self.heap_bwd.pop().unwrap();
                if self.is_settled_bwd(curr.node_id) {
                    continue;
                }
                if curr.weight > best_weight {
                    break;
                }
                if self.is_stallable_bwd(graph, curr) {
                    continue;
                }
                let begin = graph.begin_in_edges(curr.node_id);
                let end = graph.end_in_edges(curr.node_id);
                for edge_id in begin..end {
                    let adj = graph.edges_bwd()[edge_id].adj_node;
                    let edge_weight = graph.edges_bwd()[edge_id].weight;
                    let weight = curr.weight + edge_weight;
                    if weight < self.get_weight_bwd(adj) {
                        self.set_bwd(adj, weight, false);
                        self.heap_bwd.push(HeapItem::new(weight, adj));
                    }
                }
                self.settle_bwd(curr.node_id);
                let fwd_weight = self.get_weight_fwd(curr.node_id);
                if fwd_weight < WEIGHT_MAX && curr.weight + fwd_weight < best_weight {
                    best_weight = curr.weight + fwd_weight;
                }
                break;
            }
        }

        if best_weight == WEIGHT_MAX { None } else { Some(best_weight) }
    }

    #[inline]
    fn get_weight_fwd(&self, node: NodeId) -> Weight {
        self.fwd.get(&node).map_or(WEIGHT_MAX, |s| s.weight)
    }

    #[inline]
    fn get_weight_bwd(&self, node: NodeId) -> Weight {
        self.bwd.get(&node).map_or(WEIGHT_MAX, |s| s.weight)
    }

    #[inline]
    fn is_settled_fwd(&self, node: NodeId) -> bool {
        self.fwd.get(&node).map_or(false, |s| s.settled)
    }

    #[inline]
    fn is_settled_bwd(&self, node: NodeId) -> bool {
        self.bwd.get(&node).map_or(false, |s| s.settled)
    }

    #[inline]
    fn set_fwd(&mut self, node: NodeId, weight: Weight, settled: bool) {
        self.fwd.insert(node, NodeState { weight, settled });
    }

    #[inline]
    fn set_bwd(&mut self, node: NodeId, weight: Weight, settled: bool) {
        self.bwd.insert(node, NodeState { weight, settled });
    }

    #[inline]
    fn settle_fwd(&mut self, node: NodeId) {
        if let Some(s) = self.fwd.get_mut(&node) {
            s.settled = true;
        }
    }

    #[inline]
    fn settle_bwd(&mut self, node: NodeId) {
        if let Some(s) = self.bwd.get_mut(&node) {
            s.settled = true;
        }
    }

    fn is_stallable_fwd<G: FastGraph>(&self, graph: &G, curr: HeapItem) -> bool {
        let begin = graph.begin_in_edges(curr.node_id);
        let end = graph.end_in_edges(curr.node_id);
        for edge_id in begin..end {
            let adj = graph.edges_bwd()[edge_id].adj_node;
            let adj_weight = self.get_weight_fwd(adj);
            if adj_weight == WEIGHT_MAX { continue; }
            let edge_weight = graph.edges_bwd()[edge_id].weight;
            if adj_weight + edge_weight < curr.weight { return true; }
        }
        false
    }

    fn is_stallable_bwd<G: FastGraph>(&self, graph: &G, curr: HeapItem) -> bool {
        let begin = graph.begin_out_edges(curr.node_id);
        let end = graph.end_out_edges(curr.node_id);
        for edge_id in begin..end {
            let adj = graph.edges_fwd()[edge_id].adj_node;
            let adj_weight = self.get_weight_bwd(adj);
            if adj_weight == WEIGHT_MAX { continue; }
            let edge_weight = graph.edges_fwd()[edge_id].weight;
            if adj_weight + edge_weight < curr.weight { return true; }
        }
        false
    }
}
