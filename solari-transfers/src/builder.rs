use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    mem::size_of,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    sync::Arc,
};

use anyhow::{Context, bail};
use fast_paths::{FastGraph, FastGraphBuilder, InputGraph};
use geo::{Geodesic, Length};
use rayon::prelude::*;
use redb::{Database, ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};
use solari_spatial::cell_id_for_coord;
use tracing::{info, warn};
use valhalla_graphtile::{
    Access, GraphId, RoadUse,
    graph_tile::DirectedEdge,
    tile_hierarchy::STANDARD_LEVELS,
    tile_provider::{DirectoryTileProvider, GraphTileProvider, GraphTileProviderError},
};

use crate::{EDGE_LENGTH_TABLE, EDGE_SHAPE_TABLE};

const BUILD_VERSION: u32 = 1;
const RAW_TILE_SHARD_SIZE: usize = 512;
const POINT_BUCKET_BITS: u32 = 12;
const POINT_BUCKET_COUNT: usize = 1 << POINT_BUCKET_BITS;
const MAX_RAW_NODE_MAPPING_WORKERS: usize = 16;
const MAX_TRANSLATION_WORKERS: usize = 8;
const MAX_OPEN_POINT_BUCKET_WRITERS: usize = 32;
const MAX_METADATA_PREP_WORKERS: usize = 8;
const MAX_NODE_INDEX_BUCKET_WORKERS: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TileManifestEntry {
    tile_graph_id: u64,
    node_count: u32,
    raw_offset: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildManifest {
    version: u32,
    valhalla_tile_dir: String,
    total_raw_nodes: u32,
    tiles: Vec<TileManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreContractComplete {
    version: u32,
    compact_node_count: u32,
    raw_shard_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReductionStats {
    pub compact_node_count: u32,
    pub edge_count: u64,
    pub nodes_with_any_edge: u64,
    pub zero_degree_nodes: u64,
    pub dead_end_nodes: u64,
    pub simple_chain_nodes: u64,
    pub branch_nodes: u64,
    pub self_loop_nodes: u64,
    pub max_in_degree: u32,
    pub max_out_degree: u32,
    pub max_distinct_neighbors: u32,
    pub estimated_keep_nodes: u64,
    pub estimated_collapsible_nodes: u64,
    pub estimated_keep_ratio: f64,
    pub estimated_collapsible_ratio: f64,
    pub top_degree_patterns: Vec<DegreePatternCount>,
    pub top_neighbor_patterns: Vec<NeighborPatternCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegreePatternCount {
    pub in_degree: u32,
    pub out_degree: u32,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborPatternCount {
    pub distinct_neighbors: u32,
    pub in_degree: u32,
    pub out_degree: u32,
    pub count: u64,
}

#[derive(Debug, Clone, Copy)]
struct RawGraphEdgeRecord {
    from_raw: u32,
    to_raw: u32,
    weight_mm: u32,
}

#[derive(Debug, Clone, Copy)]
struct PointRecord {
    cell: u64,
    raw_node: u32,
}

#[derive(Debug, Clone, Copy)]
struct RawMetadataRecord {
    from_raw: u32,
    to_raw: u32,
    length_meters: f64,
    shape_offset: u64,
    shape_len: u32,
}

#[derive(Debug, Clone, Copy)]
struct CompactGraphEdgeRecord {
    from: u32,
    to: u32,
    weight_mm: u32,
}

#[derive(Debug, Clone, Copy)]
struct CompactPointRecord {
    cell: u64,
    compact_node: u32,
}

#[derive(Debug, Clone, Copy)]
struct CompactMetadataRecord {
    from: u32,
    to: u32,
    length_meters: f64,
    shape_offset: u64,
    shape_len: u32,
}

#[derive(Debug, Clone)]
struct RawShardSpec {
    index: usize,
    start_tile: usize,
    end_tile: usize,
}

#[derive(Debug)]
struct RawShardPaths {
    dir: PathBuf,
    done: PathBuf,
    graph_edges: PathBuf,
    points: PathBuf,
    metadata_records: PathBuf,
    metadata_shapes: PathBuf,
}

#[derive(Debug)]
struct TranslatedShardPaths {
    dir: PathBuf,
    done: PathBuf,
    compact_graph_edges: PathBuf,
    point_bucket_dir: PathBuf,
}

#[derive(Debug)]
struct PreparedMetadataShardPaths {
    dir: PathBuf,
    done: PathBuf,
    records: PathBuf,
    shapes: PathBuf,
}

#[derive(Debug)]
struct NodeIndexBucketPaths {
    dir: PathBuf,
    done: PathBuf,
    cells: PathBuf,
    data: PathBuf,
    count: PathBuf,
}

#[derive(Debug)]
struct RawNodeMapping {
    bits: Vec<u64>,
    prefix_counts: Vec<u32>,
    compact_node_count: u32,
}

pub struct PreparedTransferGraph {
    output_dir: PathBuf,
    build_dir: PathBuf,
    compact_node_count: u32,
}

pub struct TransferGraphBuildArtifacts;

impl TransferGraphBuildArtifacts {
    pub fn prepare(
        valhalla_tile_dir: &PathBuf,
        output_dir: &PathBuf,
    ) -> Result<PreparedTransferGraph, anyhow::Error> {
        let build_dir = output_dir.join(".build/precontract-v1");
        fs::create_dir_all(output_dir)?;
        fs::create_dir_all(&build_dir)?;

        let manifest_path = build_dir.join("manifest.json");
        let manifest = if manifest_path.exists() {
            let manifest: BuildManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
            if manifest.version != BUILD_VERSION
                || manifest.valhalla_tile_dir != valhalla_tile_dir.display().to_string()
            {
                info!(
                    "staged build manifest mismatch, clearing existing pre-contraction build state"
                );
                Self::reset_output_state(output_dir, &build_dir)?;
                fs::create_dir_all(&build_dir)?;
                let manifest = discover_manifest(valhalla_tile_dir)?;
                fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
                manifest
            } else {
                manifest
            }
        } else {
            let manifest = discover_manifest(valhalla_tile_dir)?;
            fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
            manifest
        };

        let complete_path = build_dir.join("precontract.complete.json");
        if complete_path.exists()
            && output_dir.join("graph_metadata.db").exists()
            && output_dir.join("transfer_node_index.bin").exists()
        {
            let complete: PreContractComplete =
                serde_json::from_slice(&fs::read(&complete_path)?)?;
            return Ok(PreparedTransferGraph {
                output_dir: output_dir.clone(),
                build_dir,
                compact_node_count: complete.compact_node_count,
            });
        }

        let tile_offsets = manifest
            .tiles
            .iter()
            .map(|entry| (entry.tile_graph_id, entry.raw_offset))
            .collect::<HashMap<_, _>>();
        let tile_offsets = Arc::new(tile_offsets);
        let shard_specs = build_raw_shard_specs(&manifest);

        info!(
            "extracting raw transfer graph shards in parallel ({} shards)",
            shard_specs.len()
        );
        let extracted_shards = AtomicUsize::new(0);
        shard_specs
            .par_iter()
            .try_for_each(|spec| -> Result<(), anyhow::Error> {
                let shard_paths = raw_shard_paths(&build_dir, spec.index);
                if !shard_paths.done.exists() {
                    extract_raw_shard(
                        valhalla_tile_dir,
                        &manifest,
                        &tile_offsets,
                        spec,
                        &shard_paths,
                    )?;
                }
                let completed = extracted_shards.fetch_add(1, Ordering::Relaxed) + 1;
                if completed % 8 == 0 || completed == shard_specs.len() {
                    info!(completed_shards = completed, total_shards = shard_specs.len(), "raw shard extraction progress");
                }
                Ok(())
            })?;

        info!("building compact raw-node mapping from extracted shards");
        let mapping = build_raw_node_mapping(&manifest, &build_dir, &shard_specs)?;
        info!(
            "raw-node compaction complete: compact_nodes={}",
            mapping.compact_node_count
        );

        info!("translating raw shards into compact edge shards and point fragments");
        let mapping = Arc::new(mapping);
        let translated_shards = AtomicUsize::new(0);
        let translation_workers = shard_specs
            .len()
            .min(rayon::current_num_threads())
            .min(MAX_TRANSLATION_WORKERS)
            .max(1);
        let translation_chunk_size = shard_specs.len().div_ceil(translation_workers).max(1);
        shard_specs
            .par_chunks(translation_chunk_size)
            .try_for_each(|chunk| -> Result<(), anyhow::Error> {
                for spec in chunk {
                    let raw_paths = raw_shard_paths(&build_dir, spec.index);
                    let translated_paths = translated_shard_paths(&build_dir, spec.index);
                    if !translated_paths.done.exists() {
                        translate_raw_shard(&raw_paths, &translated_paths, &mapping)?;
                    }
                    let completed = translated_shards.fetch_add(1, Ordering::Relaxed) + 1;
                    if completed % 8 == 0 || completed == shard_specs.len() {
                        info!(
                            completed_shards = completed,
                            total_shards = shard_specs.len(),
                            translation_workers,
                            "raw shard translation progress"
                        );
                    }
                }
                Ok(())
            })?;

        let metadata_complete = build_dir.join("metadata.complete");
        if !metadata_complete.exists() {
            info!("building graph metadata database from raw metadata shards");
            build_metadata_database(output_dir, &build_dir, &shard_specs, &mapping)?;
            fs::write(&metadata_complete, b"ok")?;
        }

        let node_index_complete = build_dir.join("node-index.complete");
        if !node_index_complete.exists() {
            info!("building transfer node index from point fragments");
            build_transfer_node_index(output_dir, &build_dir, &shard_specs)?;
            fs::write(&node_index_complete, b"ok")?;
        }

        let complete = PreContractComplete {
            version: BUILD_VERSION,
            compact_node_count: mapping.compact_node_count,
            raw_shard_count: shard_specs.len(),
        };
        fs::write(&complete_path, serde_json::to_vec_pretty(&complete)?)?;

        Ok(PreparedTransferGraph {
            output_dir: output_dir.clone(),
            build_dir,
            compact_node_count: mapping.compact_node_count,
        })
    }

    fn reset_output_state(output_dir: &Path, build_dir: &Path) -> Result<(), anyhow::Error> {
        if build_dir.exists() {
            fs::remove_dir_all(build_dir)?;
        }
        for artifact in ["graph_metadata.db", "transfer_graph.bin", "transfer_node_index.bin"] {
            let path = output_dir.join(artifact);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

impl PreparedTransferGraph {
    pub fn contract_and_write_graph(&self) -> Result<(), anyhow::Error> {
        let transfer_graph_path = self.output_dir.join("transfer_graph.bin");
        if transfer_graph_path.exists() {
            info!(
                "transfer_graph.bin already exists, skipping contraction stage at seam reuse"
            );
            return Ok(());
        }

        info!("loading compact edge shards into input graph");
        let mut graph = InputGraph::new();
        let shard_count = read_shard_count(&self.build_dir)?;
        for shard_index in 0..shard_count {
            let translated_paths = translated_shard_paths(&self.build_dir, shard_index);
            let mut reader = BufReader::new(File::open(&translated_paths.compact_graph_edges)?);
            while let Some(edge) = CompactGraphEdgeRecord::read_from(&mut reader)? {
                graph.add_edge(edge.from as usize, edge.to as usize, edge.weight_mm as usize);
            }
        }
        info!("freezing graph before contraction");
        graph.freeze();
        info!("contracting graph");
        let graph = FastGraphBuilder::build_owned(graph);
        let tmp_path = self.output_dir.join("transfer_graph.bin.tmp");
        if tmp_path.exists() {
            fs::remove_file(&tmp_path)?;
        }
        graph.save_static(tmp_path.clone())?;
        fs::rename(tmp_path, transfer_graph_path)?;
        Ok(())
    }

    pub fn compact_node_count(&self) -> u32 {
        self.compact_node_count
    }

    pub fn analyze_reduction_potential(&self) -> Result<ReductionStats, anyhow::Error> {
        info!("analyzing compact graph reduction potential");
        let node_count = self.compact_node_count as usize;
        let mut in_degree = vec![0u32; node_count];
        let mut out_degree = vec![0u32; node_count];
        let mut self_loop = vec![false; node_count];
        let mut edge_count = 0u64;

        let shard_count = read_shard_count(&self.build_dir)?;
        for shard_index in 0..shard_count {
            let translated_paths = translated_shard_paths(&self.build_dir, shard_index);
            let mut reader = BufReader::new(File::open(&translated_paths.compact_graph_edges)?);
            while let Some(edge) = CompactGraphEdgeRecord::read_from(&mut reader)? {
                let from = edge.from as usize;
                let to = edge.to as usize;
                out_degree[from] = out_degree[from].saturating_add(1);
                in_degree[to] = in_degree[to].saturating_add(1);
                if from == to {
                    self_loop[from] = true;
                }
                edge_count += 1;
            }
        }

        let mut distinct_neighbors = vec![0u32; node_count];
        for shard_index in 0..shard_count {
            let translated_paths = translated_shard_paths(&self.build_dir, shard_index);
            let mut reader = BufReader::new(File::open(&translated_paths.compact_graph_edges)?);
            let mut current_node = None::<u32>;
            let mut neighbors = HashSet::<u32>::new();
            while let Some(edge) = CompactGraphEdgeRecord::read_from(&mut reader)? {
                if current_node != Some(edge.from) {
                    if let Some(node) = current_node {
                        distinct_neighbors[node as usize] = distinct_neighbors[node as usize]
                            .saturating_add(u32::try_from(neighbors.len()).unwrap_or(u32::MAX));
                    }
                    current_node = Some(edge.from);
                    neighbors.clear();
                }
                neighbors.insert(edge.to);
            }
            if let Some(node) = current_node {
                distinct_neighbors[node as usize] = distinct_neighbors[node as usize]
                    .saturating_add(u32::try_from(neighbors.len()).unwrap_or(u32::MAX));
            }
        }
        for shard_index in 0..shard_count {
            let translated_paths = translated_shard_paths(&self.build_dir, shard_index);
            let mut reader = BufReader::new(File::open(&translated_paths.compact_graph_edges)?);
            let mut current_node = None::<u32>;
            let mut neighbors = HashSet::<u32>::new();
            while let Some(edge) = CompactGraphEdgeRecord::read_from(&mut reader)? {
                if current_node != Some(edge.to) {
                    if let Some(node) = current_node {
                        distinct_neighbors[node as usize] = distinct_neighbors[node as usize]
                            .saturating_add(u32::try_from(neighbors.len()).unwrap_or(u32::MAX));
                    }
                    current_node = Some(edge.to);
                    neighbors.clear();
                }
                neighbors.insert(edge.from);
            }
            if let Some(node) = current_node {
                distinct_neighbors[node as usize] = distinct_neighbors[node as usize]
                    .saturating_add(u32::try_from(neighbors.len()).unwrap_or(u32::MAX));
            }
        }

        let mut nodes_with_any_edge = 0u64;
        let mut zero_degree_nodes = 0u64;
        let mut dead_end_nodes = 0u64;
        let mut simple_chain_nodes = 0u64;
        let mut branch_nodes = 0u64;
        let mut self_loop_nodes = 0u64;
        let mut max_in_degree = 0u32;
        let mut max_out_degree = 0u32;
        let mut max_distinct_neighbors = 0u32;
        let mut degree_patterns = HashMap::<(u32, u32), u64>::new();
        let mut neighbor_patterns = HashMap::<(u32, u32, u32), u64>::new();

        for node in 0..node_count {
            let indeg = in_degree[node];
            let outdeg = out_degree[node];
            let neighbors = distinct_neighbors[node];
            *degree_patterns.entry((indeg, outdeg)).or_insert(0) += 1;
            *neighbor_patterns.entry((neighbors, indeg, outdeg)).or_insert(0) += 1;
            max_in_degree = max_in_degree.max(indeg);
            max_out_degree = max_out_degree.max(outdeg);
            max_distinct_neighbors = max_distinct_neighbors.max(neighbors);
            if self_loop[node] {
                self_loop_nodes += 1;
            }
            if indeg == 0 && outdeg == 0 {
                zero_degree_nodes += 1;
                continue;
            }
            nodes_with_any_edge += 1;
            if indeg == 0 || outdeg == 0 {
                dead_end_nodes += 1;
                continue;
            }
            if self_loop[node] {
                branch_nodes += 1;
                continue;
            }
            if neighbors == 2 {
                simple_chain_nodes += 1;
            } else if neighbors > 2 {
                branch_nodes += 1;
            } else {
                dead_end_nodes += 1;
            }
        }

        let estimated_collapsible_nodes = simple_chain_nodes;
        let estimated_keep_nodes = (node_count as u64).saturating_sub(estimated_collapsible_nodes);
        let estimated_keep_ratio = if node_count == 0 {
            0.0
        } else {
            estimated_keep_nodes as f64 / node_count as f64
        };
        let estimated_collapsible_ratio = if node_count == 0 {
            0.0
        } else {
            estimated_collapsible_nodes as f64 / node_count as f64
        };
        let mut top_degree_patterns = degree_patterns
            .into_iter()
            .map(|((in_degree, out_degree), count)| DegreePatternCount {
                in_degree,
                out_degree,
                count,
            })
            .collect::<Vec<_>>();
        top_degree_patterns.sort_by(|a, b| b.count.cmp(&a.count));
        top_degree_patterns.truncate(20);

        let mut top_neighbor_patterns = neighbor_patterns
            .into_iter()
            .map(|((distinct_neighbors, in_degree, out_degree), count)| NeighborPatternCount {
                distinct_neighbors,
                in_degree,
                out_degree,
                count,
            })
            .collect::<Vec<_>>();
        top_neighbor_patterns.sort_by(|a, b| b.count.cmp(&a.count));
        top_neighbor_patterns.truncate(20);

        Ok(ReductionStats {
            compact_node_count: self.compact_node_count,
            edge_count,
            nodes_with_any_edge,
            zero_degree_nodes,
            dead_end_nodes,
            simple_chain_nodes,
            branch_nodes,
            self_loop_nodes,
            max_in_degree,
            max_out_degree,
            max_distinct_neighbors,
            estimated_keep_nodes,
            estimated_collapsible_nodes,
            estimated_keep_ratio,
            estimated_collapsible_ratio,
            top_degree_patterns,
            top_neighbor_patterns,
        })
    }
}

fn discover_manifest(valhalla_tile_dir: &PathBuf) -> Result<BuildManifest, anyhow::Error> {
    let reader = DirectoryTileProvider::new(valhalla_tile_dir.clone(), NonZeroUsize::new(25).unwrap());
    let mut tiles = Vec::new();
    let mut total_raw_nodes = 0u64;
    for level in &*STANDARD_LEVELS {
        let n_tiles = level.tiling_system.n_rows * level.tiling_system.n_cols;
        for tile_id in 0..n_tiles {
            let graph_id = GraphId::try_from_components(level.level, u64::from(tile_id), 0)?;
            match reader.get_tile_containing(&graph_id) {
                Ok(tile) => {
                    let node_count = tile.header.node_count() as u64;
                    tiles.push(TileManifestEntry {
                        tile_graph_id: graph_id.value(),
                        node_count: u32::try_from(node_count)
                            .context("tile node count exceeds u32")?,
                        raw_offset: u32::try_from(total_raw_nodes)
                            .context("raw node space exceeds u32")?,
                    });
                    total_raw_nodes += node_count;
                }
                Err(GraphTileProviderError::TileDoesNotExist) => {}
                Err(err) => return Err(err.into()),
            }
        }
    }

    let total_raw_nodes =
        u32::try_from(total_raw_nodes).context("total raw node count exceeds u32")?;
    info!(
        "discovered {} tiles containing {} raw valhalla nodes",
        tiles.len(),
        total_raw_nodes
    );
    Ok(BuildManifest {
        version: BUILD_VERSION,
        valhalla_tile_dir: valhalla_tile_dir.display().to_string(),
        total_raw_nodes,
        tiles,
    })
}

fn build_raw_shard_specs(manifest: &BuildManifest) -> Vec<RawShardSpec> {
    manifest
        .tiles
        .chunks(RAW_TILE_SHARD_SIZE)
        .enumerate()
        .map(|(index, chunk)| RawShardSpec {
            index,
            start_tile: index * RAW_TILE_SHARD_SIZE,
            end_tile: index * RAW_TILE_SHARD_SIZE + chunk.len(),
        })
        .collect()
}

fn raw_shard_paths(build_dir: &Path, shard_index: usize) -> RawShardPaths {
    let dir = build_dir.join("raw-shards").join(format!("{shard_index:05}"));
    RawShardPaths {
        done: dir.join("done"),
        graph_edges: dir.join("graph_edges.bin"),
        points: dir.join("points.bin"),
        metadata_records: dir.join("metadata_records.bin"),
        metadata_shapes: dir.join("metadata_shapes.bin"),
        dir,
    }
}

fn translated_shard_paths(build_dir: &Path, shard_index: usize) -> TranslatedShardPaths {
    let dir = build_dir.join("translated-shards").join(format!("{shard_index:05}"));
    let point_bucket_dir = dir.join("point-buckets");
    TranslatedShardPaths {
        done: dir.join("done"),
        compact_graph_edges: dir.join("compact_graph_edges.bin"),
        point_bucket_dir,
        dir,
    }
}

fn prepared_metadata_shard_paths(
    build_dir: &Path,
    shard_index: usize,
) -> PreparedMetadataShardPaths {
    let dir = build_dir
        .join("prepared-metadata-shards")
        .join(format!("{shard_index:05}"));
    PreparedMetadataShardPaths {
        done: dir.join("done"),
        records: dir.join("records.bin"),
        shapes: dir.join("shapes.bin"),
        dir,
    }
}

fn node_index_bucket_paths(build_dir: &Path, bucket: u16) -> NodeIndexBucketPaths {
    let dir = build_dir.join("node-index-buckets").join(format!("{bucket:04x}"));
    NodeIndexBucketPaths {
        done: dir.join("done"),
        cells: dir.join("cells.bin"),
        data: dir.join("data.bin"),
        count: dir.join("count.bin"),
        dir,
    }
}

fn read_shard_count(build_dir: &Path) -> Result<usize, anyhow::Error> {
    let complete_path = build_dir.join("precontract.complete.json");
    let complete: PreContractComplete = serde_json::from_slice(&fs::read(complete_path)?)?;
    Ok(complete.raw_shard_count)
}

fn should_skip_edge(edge: &DirectedEdge) -> bool {
    edge.is_transit_line() || edge.is_shortcut() || edge.edge_use() == RoadUse::Ferry
}

fn tile_base_graph_id(id: &GraphId) -> Result<GraphId, anyhow::Error> {
    Ok(GraphId::try_from_components(id.level(), id.tile_id(), 0)?)
}

fn extract_raw_shard(
    valhalla_tile_dir: &PathBuf,
    manifest: &BuildManifest,
    tile_offsets: &HashMap<u64, u32>,
    spec: &RawShardSpec,
    paths: &RawShardPaths,
) -> Result<(), anyhow::Error> {
    if paths.dir.exists() {
        fs::remove_dir_all(&paths.dir)?;
    }
    fs::create_dir_all(&paths.dir)?;
    let reader = DirectoryTileProvider::new(valhalla_tile_dir.clone(), NonZeroUsize::new(25).unwrap());
    let mut graph_edge_writer = BufWriter::new(File::create(&paths.graph_edges)?);
    let mut point_writer = BufWriter::new(File::create(&paths.points)?);
    let mut metadata_record_writer = BufWriter::new(File::create(&paths.metadata_records)?);
    let mut metadata_shape_writer = BufWriter::new(File::create(&paths.metadata_shapes)?);
    let mut shape_offset = 0u64;

    for entry in &manifest.tiles[spec.start_tile..spec.end_tile] {
        let tile_graph_id = GraphId::try_from_id(entry.tile_graph_id)?;
        let tile = reader.get_tile_containing(&tile_graph_id)?;
        for local_index in 0..entry.node_count as usize {
            let node_id = tile_graph_id.with_index(local_index as u64)?;
            let node = tile.get_node(&node_id)?;
            if !node.access().contains(Access::Pedestrian) {
                continue;
            }
            let start_raw = entry.raw_offset + local_index as u32;
            for outbound_edge_index in 0..node.edge_count() {
                let edge_index = node.edge_index() + outbound_edge_index as u32;
                let edge_id = match tile_graph_id.with_index(edge_index as u64) {
                    Ok(id) => id,
                    Err(_) => {
                        warn!("edge id not constructed correctly while extracting raw shard");
                        continue;
                    }
                };
                let edge = match tile.get_directed_edge(&edge_id) {
                    Ok(edge) => edge,
                    Err(_) => {
                        warn!("directed edge not found while extracting raw shard");
                        continue;
                    }
                };
                if should_skip_edge(edge) {
                    continue;
                }
                let edge_info = match tile.get_edge_info(edge) {
                    Ok(edge_info) => edge_info,
                    Err(_) => {
                        warn!("edge info not found while extracting raw shard");
                        continue;
                    }
                };
                let shape = edge_info.shape()?.clone();
                for coord in &shape.0 {
                    PointRecord {
                        cell: cell_id_for_coord(coord),
                        raw_node: start_raw,
                    }
                    .write_to(&mut point_writer)?;
                }
                let end_node_id = edge.end_node_id();
                let end_tile_base = tile_base_graph_id(&end_node_id)?;
                let Some(end_tile_offset) = tile_offsets.get(&end_tile_base.value()) else {
                    warn!("end tile offset missing for edge to {}", end_node_id.value());
                    continue;
                };
                let end_raw = *end_tile_offset + end_node_id.index() as u32;
                if end_raw == start_raw {
                    continue;
                }

                let length_meters = shape.length::<Geodesic>();
                let weight_mm = f64_to_u32(length_meters * 1000.0)?;
                let encoded_shape = polyline::encode_coordinates(shape.0.iter().copied(), 5)?;
                let shape_len = u32::try_from(encoded_shape.len())
                    .context("encoded polyline length exceeds u32")?;
                RawMetadataRecord {
                    from_raw: start_raw,
                    to_raw: end_raw,
                    length_meters,
                    shape_offset,
                    shape_len,
                }
                .write_to(&mut metadata_record_writer)?;
                metadata_shape_writer.write_all(encoded_shape.as_bytes())?;
                shape_offset += encoded_shape.len() as u64;

                if edge.forward_access().contains(Access::Pedestrian) {
                    RawGraphEdgeRecord {
                        from_raw: start_raw,
                        to_raw: end_raw,
                        weight_mm,
                    }
                    .write_to(&mut graph_edge_writer)?;
                }
                if edge.reverse_access().contains(Access::Pedestrian) {
                    RawGraphEdgeRecord {
                        from_raw: end_raw,
                        to_raw: start_raw,
                        weight_mm,
                    }
                    .write_to(&mut graph_edge_writer)?;
                }
            }
        }
    }

    graph_edge_writer.flush()?;
    point_writer.flush()?;
    metadata_record_writer.flush()?;
    metadata_shape_writer.flush()?;
    fs::write(&paths.done, b"ok")?;
    Ok(())
}

fn build_raw_node_mapping(
    manifest: &BuildManifest,
    build_dir: &Path,
    shard_specs: &[RawShardSpec],
) -> Result<RawNodeMapping, anyhow::Error> {
    let bit_words = (manifest.total_raw_nodes as usize).div_ceil(64);
    let bit_words = bit_words.max(1);
    let mapping_workers = shard_specs
        .len()
        .min(rayon::current_num_threads())
        .min(MAX_RAW_NODE_MAPPING_WORKERS)
        .max(1);
    let shard_chunk_size = shard_specs.len().div_ceil(mapping_workers).max(1);
    let scanned_shards = AtomicUsize::new(0);

    let partial_bitsets = shard_specs
        .par_chunks(shard_chunk_size)
        .map(|shard_chunk| -> Result<Vec<u64>, anyhow::Error> {
            let mut local_bits = vec![0u64; bit_words];
            for shard in shard_chunk {
                let raw_paths = raw_shard_paths(build_dir, shard.index);
                let mut edge_reader = BufReader::new(File::open(&raw_paths.graph_edges)?);
                while let Some(edge) = RawGraphEdgeRecord::read_from(&mut edge_reader)? {
                    mark_raw_node(&mut local_bits, edge.from_raw);
                    mark_raw_node(&mut local_bits, edge.to_raw);
                }
                let mut point_reader = BufReader::new(File::open(&raw_paths.points)?);
                while let Some(point) = PointRecord::read_from(&mut point_reader)? {
                    mark_raw_node(&mut local_bits, point.raw_node);
                }
            }
            let completed = scanned_shards.fetch_add(shard_chunk.len(), Ordering::Relaxed)
                + shard_chunk.len();
            info!(
                completed_shards = completed,
                total_shards = shard_specs.len(),
                mapping_workers,
                "raw-node mapping scan progress"
            );
            Ok(local_bits)
        })
        .collect::<Vec<_>>();

    let mut bits = vec![0u64; bit_words];
    for partial_bits in partial_bitsets {
        let partial_bits = partial_bits?;
        for (dst, src) in bits.iter_mut().zip(partial_bits.into_iter()) {
            *dst |= src;
        }
    }

    let mut prefix_counts = vec![0u32; bits.len()];
    let mut total = 0u32;
    for (index, word) in bits.iter().enumerate() {
        prefix_counts[index] = total;
        total = total
            .checked_add(word.count_ones())
            .context("compact node count overflowed u32")?;
    }

    Ok(RawNodeMapping {
        bits,
        prefix_counts,
        compact_node_count: total,
    })
}

fn translate_raw_shard(
    raw_paths: &RawShardPaths,
    translated_paths: &TranslatedShardPaths,
    mapping: &RawNodeMapping,
) -> Result<(), anyhow::Error> {
    if translated_paths.dir.exists() {
        fs::remove_dir_all(&translated_paths.dir)?;
    }
    fs::create_dir_all(&translated_paths.point_bucket_dir)?;
    let mut edge_reader = BufReader::new(File::open(&raw_paths.graph_edges)?);
    let mut compact_edge_writer = BufWriter::new(File::create(&translated_paths.compact_graph_edges)?);
    while let Some(edge) = RawGraphEdgeRecord::read_from(&mut edge_reader)? {
        let Some(from) = mapping.compact_node_id(edge.from_raw) else {
            continue;
        };
        let Some(to) = mapping.compact_node_id(edge.to_raw) else {
            continue;
        };
        CompactGraphEdgeRecord {
            from,
            to,
            weight_mm: edge.weight_mm,
        }
        .write_to(&mut compact_edge_writer)?;
    }
    compact_edge_writer.flush()?;

    let mut bucket_writers = HashMap::<u16, BufWriter<File>>::new();
    let mut point_reader = BufReader::new(File::open(&raw_paths.points)?);
    while let Some(point) = PointRecord::read_from(&mut point_reader)? {
        let Some(compact_node) = mapping.compact_node_id(point.raw_node) else {
            continue;
        };
        let bucket = point_bucket(point.cell);
        if !bucket_writers.contains_key(&bucket)
            && bucket_writers.len() >= MAX_OPEN_POINT_BUCKET_WRITERS
        {
            let bucket_to_close = bucket_writers
                .keys()
                .next()
                .copied()
                .expect("point bucket writer cache unexpectedly empty");
            if let Some(mut writer) = bucket_writers.remove(&bucket_to_close) {
                writer.flush()?;
            }
        }
        if !bucket_writers.contains_key(&bucket) {
            let path = translated_paths.point_bucket_dir.join(format!("{bucket:04x}.bin"));
            let file = File::options().create(true).append(true).open(path)?;
            bucket_writers.insert(bucket, BufWriter::new(file));
        }
        let writer = bucket_writers
            .get_mut(&bucket)
            .expect("point bucket writer missing after insertion");
        CompactPointRecord {
            cell: point.cell,
            compact_node,
        }
        .write_to(writer)?;
    }
    for (_, mut writer) in bucket_writers {
        writer.flush()?;
    }

    fs::write(&translated_paths.done, b"ok")?;
    Ok(())
}

fn build_metadata_database(
    output_dir: &Path,
    build_dir: &Path,
    shard_specs: &[RawShardSpec],
    mapping: &RawNodeMapping,
) -> Result<(), anyhow::Error> {
    let metadata_workers = shard_specs
        .len()
        .min(rayon::current_num_threads())
        .min(MAX_METADATA_PREP_WORKERS)
        .max(1);
    let metadata_chunk_size = shard_specs.len().div_ceil(metadata_workers).max(1);
    let prepared_shards = AtomicUsize::new(0);
    shard_specs
        .par_chunks(metadata_chunk_size)
        .try_for_each(|chunk| -> Result<(), anyhow::Error> {
            for shard in chunk {
                let raw_paths = raw_shard_paths(build_dir, shard.index);
                let prepared_paths = prepared_metadata_shard_paths(build_dir, shard.index);
                if !prepared_paths.done.exists() {
                    prepare_metadata_shard(&raw_paths, &prepared_paths, mapping, shard.index)?;
                }
                let completed = prepared_shards.fetch_add(1, Ordering::Relaxed) + 1;
                if completed % 8 == 0 || completed == shard_specs.len() {
                    info!(
                        completed_shards = completed,
                        total_shards = shard_specs.len(),
                        metadata_workers,
                        "prepared metadata shards progress"
                    );
                }
            }
            Ok(())
        })?;

    let database_path = output_dir.join("graph_metadata.db");
    if database_path.exists() {
        fs::remove_file(&database_path)?;
    }
    let database = Database::create(&database_path)?;
    for shard in shard_specs {
        let prepared_paths = prepared_metadata_shard_paths(build_dir, shard.index);
        let shapes = fs::read(&prepared_paths.shapes)?;
        let txn = database.begin_write()?;
        let mut reader = BufReader::new(File::open(&prepared_paths.records)?);
        while let Some(record) = CompactMetadataRecord::read_from(&mut reader)? {
            let start = record.shape_offset as usize;
            let end = start + record.shape_len as usize;
            if end > shapes.len() {
                bail!("metadata shape record out of bounds in shard {}", shard.index);
            }
            upsert_edge_metadata(
                &txn,
                record.from as u64,
                record.to as u64,
                record.length_meters,
                &shapes[start..end],
            )?;
        }
        txn.commit()?;
        if (shard.index + 1) % 8 == 0 || shard.index + 1 == shard_specs.len() {
            info!(
                committed_shards = shard.index + 1,
                total_shards = shard_specs.len(),
                "graph metadata database commit progress"
            );
        }
    }
    Ok(())
}

fn build_transfer_node_index(
    output_dir: &Path,
    build_dir: &Path,
    shard_specs: &[RawShardSpec],
) -> Result<(), anyhow::Error> {
    let bucket_ids = (0..POINT_BUCKET_COUNT as u16).collect::<Vec<_>>();
    let bucket_workers = bucket_ids
        .len()
        .min(rayon::current_num_threads())
        .min(MAX_NODE_INDEX_BUCKET_WORKERS)
        .max(1);
    let bucket_chunk_size = bucket_ids.len().div_ceil(bucket_workers).max(1);
    let prepared_buckets = AtomicUsize::new(0);
    bucket_ids
        .par_chunks(bucket_chunk_size)
        .try_for_each(|chunk| -> Result<(), anyhow::Error> {
            for bucket in chunk {
                let bucket_paths = node_index_bucket_paths(build_dir, *bucket);
                if !bucket_paths.done.exists() {
                    prepare_node_index_bucket(build_dir, shard_specs, *bucket, &bucket_paths)?;
                }
                let completed = prepared_buckets.fetch_add(1, Ordering::Relaxed) + 1;
                if completed % 128 == 0 || completed == bucket_ids.len() {
                    info!(
                        completed_buckets = completed,
                        total_buckets = bucket_ids.len(),
                        bucket_workers,
                        "prepared node-index bucket progress"
                    );
                }
            }
            Ok(())
        })?;

    let cells_tmp_path = output_dir.join("transfer_node_index.cells.tmp");
    let data_tmp_path = output_dir.join("transfer_node_index.data.tmp");
    let index_tmp_path = output_dir.join("transfer_node_index.bin.tmp");
    for path in [&cells_tmp_path, &data_tmp_path, &index_tmp_path] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }

    let mut cells_writer = BufWriter::new(File::create(&cells_tmp_path)?);
    let mut data_writer = BufWriter::new(File::create(&data_tmp_path)?);
    let mut total_points = 0u64;

    for bucket in 0..POINT_BUCKET_COUNT as u16 {
        let bucket_paths = node_index_bucket_paths(build_dir, bucket);
        let count = read_u64_file(&bucket_paths.count)?;
        let mut bucket_cells_reader = BufReader::new(File::open(&bucket_paths.cells)?);
        io::copy(&mut bucket_cells_reader, &mut cells_writer)?;
        let mut bucket_data_reader = BufReader::new(File::open(&bucket_paths.data)?);
        io::copy(&mut bucket_data_reader, &mut data_writer)?;
        total_points += count;
        if (bucket as usize + 1) % 128 == 0 || bucket as usize + 1 == POINT_BUCKET_COUNT {
            info!(
                appended_buckets = bucket as usize + 1,
                total_buckets = POINT_BUCKET_COUNT,
                total_points,
                "transfer node index concatenation progress"
            );
        }
    }
    cells_writer.flush()?;
    data_writer.flush()?;

    let mut index_writer = BufWriter::new(File::create(&index_tmp_path)?);
    index_writer.write_all(&total_points.to_ne_bytes())?;
    let mut cells_reader = BufReader::new(File::open(&cells_tmp_path)?);
    io::copy(&mut cells_reader, &mut index_writer)?;
    let mut data_reader = BufReader::new(File::open(&data_tmp_path)?);
    io::copy(&mut data_reader, &mut index_writer)?;
    index_writer.flush()?;

    let final_path = output_dir.join("transfer_node_index.bin");
    if final_path.exists() {
        fs::remove_file(&final_path)?;
    }
    fs::rename(index_tmp_path, final_path)?;
    fs::remove_file(cells_tmp_path)?;
    fs::remove_file(data_tmp_path)?;
    Ok(())
}

fn prepare_metadata_shard(
    raw_paths: &RawShardPaths,
    prepared_paths: &PreparedMetadataShardPaths,
    mapping: &RawNodeMapping,
    shard_index: usize,
) -> Result<(), anyhow::Error> {
    if prepared_paths.dir.exists() {
        fs::remove_dir_all(&prepared_paths.dir)?;
    }
    fs::create_dir_all(&prepared_paths.dir)?;

    let raw_shapes = fs::read(&raw_paths.metadata_shapes)?;
    let mut reader = BufReader::new(File::open(&raw_paths.metadata_records)?);
    let mut record_writer = BufWriter::new(File::create(&prepared_paths.records)?);
    let mut shape_writer = BufWriter::new(File::create(&prepared_paths.shapes)?);
    let mut shape_offset = 0u64;

    while let Some(record) = RawMetadataRecord::read_from(&mut reader)? {
        let Some(from) = mapping.compact_node_id(record.from_raw) else {
            continue;
        };
        let Some(to) = mapping.compact_node_id(record.to_raw) else {
            continue;
        };
        let start = record.shape_offset as usize;
        let end = start + record.shape_len as usize;
        if end > raw_shapes.len() {
            bail!("metadata shape record out of bounds in shard {}", shard_index);
        }
        shape_writer.write_all(&raw_shapes[start..end])?;
        CompactMetadataRecord {
            from,
            to,
            length_meters: record.length_meters,
            shape_offset,
            shape_len: record.shape_len,
        }
        .write_to(&mut record_writer)?;
        shape_offset += record.shape_len as u64;
    }

    record_writer.flush()?;
    shape_writer.flush()?;
    fs::write(&prepared_paths.done, b"ok")?;
    Ok(())
}

fn prepare_node_index_bucket(
    build_dir: &Path,
    shard_specs: &[RawShardSpec],
    bucket: u16,
    bucket_paths: &NodeIndexBucketPaths,
) -> Result<(), anyhow::Error> {
    if bucket_paths.dir.exists() {
        fs::remove_dir_all(&bucket_paths.dir)?;
    }
    fs::create_dir_all(&bucket_paths.dir)?;

    let mut points = Vec::<CompactPointRecord>::new();
    for shard in shard_specs {
        let translated_paths = translated_shard_paths(build_dir, shard.index);
        let fragment_path = translated_paths
            .point_bucket_dir
            .join(format!("{bucket:04x}.bin"));
        if !fragment_path.exists() {
            continue;
        }
        let mut reader = BufReader::new(File::open(fragment_path)?);
        while let Some(point) = CompactPointRecord::read_from(&mut reader)? {
            points.push(point);
        }
    }
    points.sort_unstable_by_key(|point| point.cell);

    let mut cells_writer = BufWriter::new(File::create(&bucket_paths.cells)?);
    let mut data_writer = BufWriter::new(File::create(&bucket_paths.data)?);
    for point in &points {
        cells_writer.write_all(&point.cell.to_ne_bytes())?;
        data_writer.write_all(&(point.compact_node as usize).to_ne_bytes())?;
    }
    cells_writer.flush()?;
    data_writer.flush()?;
    fs::write(&bucket_paths.count, (points.len() as u64).to_ne_bytes())?;
    fs::write(&bucket_paths.done, b"ok")?;
    Ok(())
}

fn read_u64_file(path: &Path) -> Result<u64, anyhow::Error> {
    let bytes = fs::read(path)?;
    if bytes.len() != size_of::<u64>() {
        bail!("expected {} bytes in {}, got {}", size_of::<u64>(), path.display(), bytes.len());
    }
    let mut buf = [0u8; size_of::<u64>()];
    buf.copy_from_slice(&bytes);
    Ok(u64::from_ne_bytes(buf))
}

fn upsert_edge_metadata(
    txn: &WriteTransaction,
    from: u64,
    to: u64,
    length: f64,
    shape_bytes: &[u8],
) -> Result<bool, anyhow::Error> {
    let key = (from, to);
    let should_insert_shape = {
        let lengths = txn.open_table(EDGE_LENGTH_TABLE)?;
        if let Some(previous_len) = lengths.get(&key)? {
            length < previous_len.value()
        } else {
            true
        }
    };
    if !should_insert_shape {
        return Ok(false);
    }
    let mut lengths = txn.open_table(EDGE_LENGTH_TABLE)?;
    lengths.insert(&key, length)?;
    let mut shapes = txn.open_table(EDGE_SHAPE_TABLE)?;
    shapes.insert(&key, shape_bytes)?;
    Ok(true)
}

fn mark_raw_node(bits: &mut [u64], raw_node: u32) {
    let raw_node = raw_node as usize;
    let word_index = raw_node / 64;
    let bit_index = raw_node % 64;
    bits[word_index] |= 1u64 << bit_index;
}

impl RawNodeMapping {
    fn compact_node_id(&self, raw_node: u32) -> Option<u32> {
        let raw_node = raw_node as usize;
        let word_index = raw_node / 64;
        let bit_index = raw_node % 64;
        let word = *self.bits.get(word_index)?;
        let bit_mask = 1u64 << bit_index;
        if word & bit_mask == 0 {
            return None;
        }
        let prefix = *self.prefix_counts.get(word_index)?;
        let within_word_mask = if bit_index == 0 {
            0
        } else {
            (1u64 << bit_index) - 1
        };
        Some(prefix + (word & within_word_mask).count_ones())
    }
}

fn point_bucket(cell: u64) -> u16 {
    (cell >> (64 - POINT_BUCKET_BITS)) as u16
}

fn f64_to_u32(value: f64) -> Result<u32, anyhow::Error> {
    if !(0.0..=(u32::MAX as f64)).contains(&value) {
        bail!("value {} cannot fit into u32", value);
    }
    Ok(value as u32)
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut read_total = 0usize;
    while read_total < buf.len() {
        match reader.read(&mut buf[read_total..])? {
            0 if read_total == 0 => return Ok(false),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF while reading record",
                ))
            }
            n => read_total += n,
        }
    }
    Ok(true)
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_ne_bytes())
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_ne_bytes())
}

fn write_f64<W: Write>(writer: &mut W, value: f64) -> io::Result<()> {
    writer.write_all(&value.to_ne_bytes())
}

fn read_u32<R: Read>(reader: &mut R) -> io::Result<Option<u32>> {
    let mut buf = [0u8; size_of::<u32>()];
    if !read_exact_or_eof(reader, &mut buf)? {
        return Ok(None);
    }
    Ok(Some(u32::from_ne_bytes(buf)))
}

fn read_u64<R: Read>(reader: &mut R) -> io::Result<Option<u64>> {
    let mut buf = [0u8; size_of::<u64>()];
    if !read_exact_or_eof(reader, &mut buf)? {
        return Ok(None);
    }
    Ok(Some(u64::from_ne_bytes(buf)))
}

fn read_f64<R: Read>(reader: &mut R) -> io::Result<Option<f64>> {
    let mut buf = [0u8; size_of::<f64>()];
    if !read_exact_or_eof(reader, &mut buf)? {
        return Ok(None);
    }
    Ok(Some(f64::from_ne_bytes(buf)))
}

impl RawGraphEdgeRecord {
    fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_u32(writer, self.from_raw)?;
        write_u32(writer, self.to_raw)?;
        write_u32(writer, self.weight_mm)
    }

    fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let Some(from_raw) = read_u32(reader)? else {
            return Ok(None);
        };
        let Some(to_raw) = read_u32(reader)? else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "graph edge truncated"));
        };
        let Some(weight_mm) = read_u32(reader)? else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "graph edge truncated"));
        };
        Ok(Some(Self {
            from_raw,
            to_raw,
            weight_mm,
        }))
    }
}

impl PointRecord {
    fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_u64(writer, self.cell)?;
        write_u32(writer, self.raw_node)
    }

    fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let Some(cell) = read_u64(reader)? else {
            return Ok(None);
        };
        let Some(raw_node) = read_u32(reader)? else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "point record truncated"));
        };
        Ok(Some(Self { cell, raw_node }))
    }
}

impl RawMetadataRecord {
    fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_u32(writer, self.from_raw)?;
        write_u32(writer, self.to_raw)?;
        write_f64(writer, self.length_meters)?;
        write_u64(writer, self.shape_offset)?;
        write_u32(writer, self.shape_len)
    }

    fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let Some(from_raw) = read_u32(reader)? else {
            return Ok(None);
        };
        let Some(to_raw) = read_u32(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "metadata record truncated",
            ));
        };
        let Some(length_meters) = read_f64(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "metadata record truncated",
            ));
        };
        let Some(shape_offset) = read_u64(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "metadata record truncated",
            ));
        };
        let Some(shape_len) = read_u32(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "metadata record truncated",
            ));
        };
        Ok(Some(Self {
            from_raw,
            to_raw,
            length_meters,
            shape_offset,
            shape_len,
        }))
    }
}

impl CompactGraphEdgeRecord {
    fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_u32(writer, self.from)?;
        write_u32(writer, self.to)?;
        write_u32(writer, self.weight_mm)
    }

    fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let Some(from) = read_u32(reader)? else {
            return Ok(None);
        };
        let Some(to) = read_u32(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compact graph edge truncated",
            ));
        };
        let Some(weight_mm) = read_u32(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compact graph edge truncated",
            ));
        };
        Ok(Some(Self {
            from,
            to,
            weight_mm,
        }))
    }
}

impl CompactPointRecord {
    fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_u64(writer, self.cell)?;
        write_u32(writer, self.compact_node)
    }

    fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let Some(cell) = read_u64(reader)? else {
            return Ok(None);
        };
        let Some(compact_node) = read_u32(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compact point truncated",
            ));
        };
        Ok(Some(Self { cell, compact_node }))
    }
}

impl CompactMetadataRecord {
    fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        write_u32(writer, self.from)?;
        write_u32(writer, self.to)?;
        write_f64(writer, self.length_meters)?;
        write_u64(writer, self.shape_offset)?;
        write_u32(writer, self.shape_len)
    }

    fn read_from<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let Some(from) = read_u32(reader)? else {
            return Ok(None);
        };
        let Some(to) = read_u32(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compact metadata record truncated",
            ));
        };
        let Some(length_meters) = read_f64(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compact metadata record truncated",
            ));
        };
        let Some(shape_offset) = read_u64(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compact metadata record truncated",
            ));
        };
        let Some(shape_len) = read_u32(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compact metadata record truncated",
            ));
        };
        Ok(Some(Self {
            from,
            to,
            length_meters,
            shape_offset,
            shape_len,
        }))
    }
}
