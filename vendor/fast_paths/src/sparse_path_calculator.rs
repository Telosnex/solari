/*
 * Sparse path calculator for contraction hierarchy queries.
 *
 * Like SparseWeightCalculator, uses HashMaps instead of dense arrays to avoid
 * allocating ~19 GB for a 479M node graph. Also tracks parent pointers and
 * incoming edges for path reconstruction, like PathCalculator.
 */

use std::collections::BinaryHeap;
use std::collections::HashMap;

use crate::constants::{EdgeId, NodeId, Weight, INVALID_EDGE, INVALID_NODE, WEIGHT_MAX};
use crate::fast_graph::FastGraph;
use crate::heap_item::HeapItem;
use crate::shortest_path::ShortestPath;

struct NodeData {
    weight: Weight,
    settled: bool,
    parent: NodeId,
    inc_edge: EdgeId,
}

pub struct SparsePathCalculator {
    num_nodes: usize,
    fwd: HashMap<NodeId, NodeData>,
    bwd: HashMap<NodeId, NodeData>,
    heap_fwd: BinaryHeap<HeapItem>,
    heap_bwd: BinaryHeap<HeapItem>,
}

impl SparsePathCalculator {
    pub fn new(num_nodes: usize) -> Self {
        SparsePathCalculator {
            num_nodes,
            fwd: HashMap::with_capacity(4096),
            bwd: HashMap::with_capacity(4096),
            heap_fwd: BinaryHeap::with_capacity(4096),
            heap_bwd: BinaryHeap::with_capacity(4096),
        }
    }

    pub fn calc_path_multiple_sources_and_targets<G: FastGraph>(
        &mut self,
        graph: &G,
        starts: Vec<(NodeId, Weight)>,
        ends: Vec<(NodeId, Weight)>,
    ) -> Option<ShortestPath> {
        assert_eq!(graph.get_num_nodes(), self.num_nodes, "given graph has invalid node count");
        assert!(!starts.is_empty(), "there has to be at least one start");
        assert!(!ends.is_empty(), "there has to be at least one end");

        self.fwd.clear();
        self.bwd.clear();
        self.heap_fwd.clear();
        self.heap_bwd.clear();

        let mut best_weight = WEIGHT_MAX;
        let mut meeting_node = INVALID_NODE;

        // Check direct source==target matches
        for (start_node, start_weight) in &starts {
            for (end_node, end_weight) in &ends {
                if *start_node == *end_node
                    && *start_weight < WEIGHT_MAX
                    && *end_weight < WEIGHT_MAX
                    && *start_weight + *end_weight < best_weight
                {
                    best_weight = *start_weight + *end_weight;
                    meeting_node = *end_node;
                }
            }
        }

        // Initialize forward search
        for (node, weight) in starts {
            if weight < self.get_weight_fwd(node) {
                self.set_fwd(node, weight, false, node, INVALID_EDGE);
                self.heap_fwd.push(HeapItem::new(weight, node));
            }
        }

        // Initialize backward search
        for (node, weight) in ends {
            if weight < self.get_weight_bwd(node) {
                self.set_bwd(node, weight, false, node, INVALID_EDGE);
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
                        self.set_fwd(adj, weight, false, curr.node_id, edge_id);
                        self.heap_fwd.push(HeapItem::new(weight, adj));
                    }
                }
                self.settle_fwd(curr.node_id);
                let bwd_weight = self.get_weight_bwd(curr.node_id);
                if bwd_weight < WEIGHT_MAX && curr.weight + bwd_weight < best_weight {
                    best_weight = curr.weight + bwd_weight;
                    meeting_node = curr.node_id;
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
                        self.set_bwd(adj, weight, false, curr.node_id, edge_id);
                        self.heap_bwd.push(HeapItem::new(weight, adj));
                    }
                }
                self.settle_bwd(curr.node_id);
                let fwd_weight = self.get_weight_fwd(curr.node_id);
                if fwd_weight < WEIGHT_MAX && curr.weight + fwd_weight < best_weight {
                    best_weight = curr.weight + fwd_weight;
                    meeting_node = curr.node_id;
                }
                break;
            }
        }

        if meeting_node == INVALID_NODE {
            None
        } else {
            assert!(best_weight < WEIGHT_MAX);
            let nodes = self.extract_nodes(graph, meeting_node);
            assert!(!nodes.is_empty());
            Some(ShortestPath::new(nodes[0], nodes[nodes.len() - 1], best_weight, nodes))
        }
    }

    fn extract_nodes<G: FastGraph>(&self, graph: &G, meeting_node: NodeId) -> Vec<NodeId> {
        assert_ne!(meeting_node, INVALID_NODE);
        let mut result = Vec::new();
        let mut node = meeting_node;
        loop {
            let data = self.fwd.get(&node).expect("fwd node must exist");
            if data.inc_edge == INVALID_EDGE {
                break;
            }
            Self::unpack_fwd(graph, &mut result, data.inc_edge, true);
            node = data.parent;
        }
        result.reverse();
        node = meeting_node;
        loop {
            let data = self.bwd.get(&node).expect("bwd node must exist");
            if data.inc_edge == INVALID_EDGE {
                break;
            }
            Self::unpack_bwd(graph, &mut result, data.inc_edge, false);
            node = data.parent;
        }
        // we stored the target node as 'parent' of the root of the shortest tree
        result.push(node);
        result
    }

    fn unpack_fwd<G: FastGraph>(
        graph: &G,
        nodes: &mut Vec<NodeId>,
        edge_id: EdgeId,
        reverse: bool,
    ) {
        if !graph.edges_fwd()[edge_id].is_shortcut() {
            nodes.push(graph.edges_fwd()[edge_id].base_node);
            return;
        }
        if reverse {
            Self::unpack_fwd(graph, nodes, graph.edges_fwd()[edge_id].replaced_out_edge, reverse);
            Self::unpack_bwd(graph, nodes, graph.edges_fwd()[edge_id].replaced_in_edge, reverse);
        } else {
            Self::unpack_bwd(graph, nodes, graph.edges_fwd()[edge_id].replaced_in_edge, reverse);
            Self::unpack_fwd(graph, nodes, graph.edges_fwd()[edge_id].replaced_out_edge, reverse);
        }
    }

    fn unpack_bwd<G: FastGraph>(
        graph: &G,
        nodes: &mut Vec<NodeId>,
        edge_id: EdgeId,
        reverse: bool,
    ) {
        if !graph.edges_bwd()[edge_id].is_shortcut() {
            nodes.push(graph.edges_bwd()[edge_id].adj_node);
            return;
        }
        if reverse {
            Self::unpack_fwd(graph, nodes, graph.edges_bwd()[edge_id].replaced_out_edge, reverse);
            Self::unpack_bwd(graph, nodes, graph.edges_bwd()[edge_id].replaced_in_edge, reverse);
        } else {
            Self::unpack_bwd(graph, nodes, graph.edges_bwd()[edge_id].replaced_in_edge, reverse);
            Self::unpack_fwd(graph, nodes, graph.edges_bwd()[edge_id].replaced_out_edge, reverse);
        }
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
    fn set_fwd(&mut self, node: NodeId, weight: Weight, settled: bool, parent: NodeId, inc_edge: EdgeId) {
        self.fwd.insert(node, NodeData { weight, settled, parent, inc_edge });
    }

    #[inline]
    fn set_bwd(&mut self, node: NodeId, weight: Weight, settled: bool, parent: NodeId, inc_edge: EdgeId) {
        self.bwd.insert(node, NodeData { weight, settled, parent, inc_edge });
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
