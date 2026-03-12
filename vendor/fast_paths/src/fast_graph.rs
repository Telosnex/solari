/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

use std::convert::TryInto;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::slice;

use anyhow::bail;
use anyhow::Ok;
use bytemuck::cast_slice;
use bytemuck::Pod;
use bytemuck::Zeroable;
use memmap2::Mmap;
use serde::Deserialize;
use serde::Serialize;

use crate::constants::Weight;
use crate::constants::{EdgeId, NodeId, INVALID_EDGE};

pub trait FastGraph {
    fn edges_fwd<'a>(&'a self) -> &'a [FastGraphEdge];
    fn edges_bwd<'a>(&'a self) -> &'a [FastGraphEdge];
    fn ranks<'a>(&'a self) -> &'a [usize];
    fn get_num_nodes(&self) -> usize;
    fn first_edge_ids_bwd<'a>(&'a self) -> &'a [EdgeId];
    fn first_edge_ids_fwd<'a>(&'a self) -> &'a [EdgeId];

    fn get_node_ordering(&self) -> Vec<NodeId> {
        let mut ordering = vec![0; self.ranks().len()];
        for i in 0..self.ranks().len() {
            ordering[self.ranks()[i]] = i;
        }
        ordering
    }

    fn get_num_out_edges(&self) -> usize {
        self.edges_fwd().len()
    }

    fn get_num_in_edges(&self) -> usize {
        self.edges_bwd().len()
    }

    fn begin_in_edges(&self, node: NodeId) -> usize {
        self.first_edge_ids_bwd()[self.ranks()[node]]
    }

    fn end_in_edges(&self, node: NodeId) -> usize {
        self.first_edge_ids_bwd()[self.ranks()[node] + 1]
    }

    fn begin_out_edges(&self, node: NodeId) -> usize {
        self.first_edge_ids_fwd()[self.ranks()[node]]
    }

    fn end_out_edges(&self, node: NodeId) -> usize {
        self.first_edge_ids_fwd()[self.ranks()[node] + 1]
    }

    fn save_static(&self, path: PathBuf) -> Result<(), anyhow::Error> {
        let mut file = File::create(path)?;
        // Write the number of nodes.
        file.write_all(&self.get_num_nodes().to_le_bytes())?;

        // Write the lengths of each slice.
        file.write_all(&(self.ranks().len() as u64).to_le_bytes())?;
        file.write_all(&(self.edges_fwd().len() as u64).to_le_bytes())?;
        file.write_all(&(self.first_edge_ids_fwd().len() as u64).to_le_bytes())?;
        file.write_all(&(self.edges_bwd().len() as u64).to_le_bytes())?;
        file.write_all(&(self.first_edge_ids_bwd().len() as u64).to_le_bytes())?;

        // Write the actual data.
        file.write_all(bytemuck::cast_slice(&self.ranks()))?;
        file.write_all(bytemuck::cast_slice(&self.edges_fwd()))?;
        file.write_all(bytemuck::cast_slice(&self.first_edge_ids_fwd()))?;
        file.write_all(bytemuck::cast_slice(&self.edges_bwd()))?;
        file.write_all(bytemuck::cast_slice(&self.first_edge_ids_bwd()))?;
        file.sync_all()?;

        Ok(())
    }
}

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FastGraphVec {
    num_nodes: usize,
    pub(crate) ranks: Vec<usize>,
    pub(crate) edges_fwd: Vec<FastGraphEdge>,
    pub(crate) first_edge_ids_fwd: Vec<EdgeId>,

    pub(crate) edges_bwd: Vec<FastGraphEdge>,
    pub(crate) first_edge_ids_bwd: Vec<EdgeId>,
}

impl FastGraphVec {
    pub fn new(num_nodes: usize) -> Self {
        FastGraphVec {
            ranks: vec![0; num_nodes],
            num_nodes,
            edges_fwd: vec![],
            first_edge_ids_fwd: vec![0; num_nodes + 1],
            edges_bwd: vec![],
            first_edge_ids_bwd: vec![0; num_nodes + 1],
        }
    }
}

#[repr(C)]
pub struct FastGraphStatic<'a> {
    mmap: Pin<Mmap>,
    num_nodes: usize,
    pub(crate) ranks: &'a [usize],
    pub(crate) edges_fwd: &'a [FastGraphEdge],
    pub(crate) first_edge_ids_fwd: &'a [EdgeId],

    pub(crate) edges_bwd: &'a [FastGraphEdge],
    pub(crate) first_edge_ids_bwd: &'a [EdgeId],
}

impl<'a> FastGraphStatic<'a> {
    pub fn assemble(mmap: Pin<Mmap>) -> Result<FastGraphStatic<'a>, anyhow::Error> {
        assert_eq!(usize::MAX as u64, u64::MAX);
        let word_size = 8;
        if mmap.len() < word_size * 6 {
            bail!("Mmap too small");
        }
        let data = &mmap;
        let num_nodes = u64::from_le_bytes(data[..word_size].try_into()?) as usize;
        let data = &data[word_size..];
        let num_ranks = u64::from_le_bytes(data[..word_size].try_into()?) as usize;
        let data = &data[word_size..];
        let num_edges_fwd = u64::from_le_bytes(data[..word_size].try_into()?) as usize;
        let data = &data[word_size..];
        let num_first_edge_ids_fwd = u64::from_le_bytes(data[..word_size].try_into()?) as usize;
        let data = &data[word_size..];
        let num_edges_bwd = u64::from_le_bytes(data[..word_size].try_into()?) as usize;
        let data = &data[word_size..];
        let num_first_edge_ids_bwd = u64::from_le_bytes(data[..word_size].try_into()?) as usize;
        let data = &data[word_size..];

        let ranks = unsafe {
            let s: &[usize] = cast_slice(&data[..num_ranks * size_of::<usize>()]);
            slice::from_raw_parts(s.as_ptr(), s.len())
        };
        let data = &data[num_ranks * size_of::<usize>()..];

        let edges_fwd = unsafe {
            let s: &[FastGraphEdge] =
                cast_slice(&data[..num_edges_fwd * size_of::<FastGraphEdge>()]);
            assert_eq!(s.len(), num_edges_fwd);
            slice::from_raw_parts(s.as_ptr(), s.len())
        };
        let data = &data[num_edges_fwd * size_of::<FastGraphEdge>()..];

        let first_edge_ids_fwd = unsafe {
            let s: &[EdgeId] = cast_slice(&data[..num_first_edge_ids_fwd * size_of::<EdgeId>()]);
            assert_eq!(s.len(), num_first_edge_ids_fwd);
            slice::from_raw_parts(s.as_ptr(), s.len())
        };
        let data = &data[num_first_edge_ids_fwd * size_of::<EdgeId>()..];

        let edges_bwd = unsafe {
            let s: &[FastGraphEdge] =
                cast_slice(&data[..num_edges_bwd * size_of::<FastGraphEdge>()]);
            slice::from_raw_parts(s.as_ptr(), s.len())
        };
        let data = &data[num_edges_bwd * size_of::<FastGraphEdge>()..];

        let first_edge_ids_bwd = unsafe {
            let s: &[EdgeId] = cast_slice(&data[..num_first_edge_ids_bwd * size_of::<EdgeId>()]);
            slice::from_raw_parts(s.as_ptr(), s.len())
        };

        Ok(FastGraphStatic {
            mmap,
            num_nodes,
            ranks,
            edges_fwd,
            first_edge_ids_fwd,
            edges_bwd,
            first_edge_ids_bwd,
        })
    }
}

impl FastGraph for FastGraphVec {
    fn edges_fwd<'a>(&'a self) -> &'a [FastGraphEdge] {
        &self.edges_fwd
    }

    fn edges_bwd<'a>(&'a self) -> &'a [FastGraphEdge] {
        &self.edges_bwd
    }

    fn ranks<'a>(&'a self) -> &'a [usize] {
        &self.ranks
    }

    fn get_num_nodes(&self) -> usize {
        self.num_nodes
    }

    fn first_edge_ids_bwd<'a>(&'a self) -> &'a [EdgeId] {
        &self.first_edge_ids_bwd
    }

    fn first_edge_ids_fwd<'a>(&'a self) -> &'a [EdgeId] {
        &self.first_edge_ids_fwd
    }
}

impl FastGraph for FastGraphStatic<'_> {
    fn edges_fwd<'a>(&'a self) -> &'a [FastGraphEdge] {
        self.edges_fwd
    }

    fn edges_bwd<'a>(&'a self) -> &'a [FastGraphEdge] {
        self.edges_bwd
    }

    fn ranks<'a>(&'a self) -> &'a [usize] {
        self.ranks
    }

    fn get_num_nodes(&self) -> usize {
        self.num_nodes
    }

    fn first_edge_ids_bwd<'a>(&'a self) -> &'a [EdgeId] {
        self.first_edge_ids_bwd
    }

    fn first_edge_ids_fwd<'a>(&'a self) -> &'a [EdgeId] {
        self.first_edge_ids_fwd
    }
}

#[repr(C)]
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Pod, Zeroable)]
pub struct FastGraphEdge {
    // todo: the base_node is 'redundant' for the routing query so to say, but makes the implementation easier for now
    // and can still be removed at a later time, we definitely need this information on original
    // edges for shortcut unpacking. a possible hack is storing it in the (for non-shortcuts)
    // unused replaced_in/out_edge field.
    pub base_node: NodeId,
    pub adj_node: NodeId,
    pub weight: Weight,
    pub replaced_in_edge: EdgeId,
    pub replaced_out_edge: EdgeId,
}

impl FastGraphEdge {
    pub fn new(
        base_node: NodeId,
        adj_node: NodeId,
        weight: Weight,
        replaced_edge1: EdgeId,
        replaced_edge2: EdgeId,
    ) -> Self {
        FastGraphEdge {
            base_node,
            adj_node,
            weight,
            replaced_in_edge: replaced_edge1,
            replaced_out_edge: replaced_edge2,
        }
    }

    pub fn is_shortcut(&self) -> bool {
        assert!(
            (self.replaced_in_edge == INVALID_EDGE && self.replaced_out_edge == INVALID_EDGE)
                || (self.replaced_in_edge != INVALID_EDGE
                    && self.replaced_out_edge != INVALID_EDGE)
        );
        self.replaced_in_edge != INVALID_EDGE
    }
}
