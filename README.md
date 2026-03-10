# Solari

Solari is a high-performance transit routing engine built using the [RAPTOR algorithm](https://www.microsoft.com/en-us/research/wp-content/uploads/2012/01/raptor_alenex.pdf), optimized for lightweight, global-scale public transit routing. Designed to serve developers building applications requiring fast and resource-efficient transit planning (e.g., maps apps, trip-planning APIs), it avoids heavy preprocessing steps while supporting planet-scale coverage through memory-mapped timetables.

## Key Features
- **Planet-Scale Coverage**:
  - Memory-mapped timetable data allows a single instance to handle global networks with minimal RAM usage (via `memmap2`).

- **Multi-Agency Support**:
  - Load multiple GTFS feeds from a directory for seamless cross-agency routing.

- **Timezone Awareness**:
  - Automatically handles timezone conversions based on GTFS feed data. Developers are responsible for converting epoch timestamps to local time in their app layer.

- **HTTP API Endpoint**:
  ```http
  POST /v1/plan
  ```
  Example request:
  ```bash
  curl -d '{"from":{"lat":47.679591,"lon":-122.356388},"to":{"lat":47.616440,"lon":-122.320440},"start_at":1742845000000}' \
       https://transit.maps.earth/v1/plan
  ```

- **GTFS Compatibility**:
  - Supports modern GTFS feeds via the `gtfs-structures` crate.
  - No real-time (GTFS-RT) support yet; prioritized roadmap features include alerts and delays.

## Getting Started

### Prerequisites
1. **Rust** (`rustc >= 1.86` tested).
2. **OpenSSL development package**: Install via your OS's package manager (e.g., `libssl-dev` on Ubuntu).

### Quickstart

Solari now needs two kinds of input data:

- GTFS feeds for the transit timetable
- a prebuilt pedestrian transfer graph derived from Valhalla tiles

That means the setup flow is:

1. get or build a directory of Valhalla tiles
2. export Solari's transfer graph from those tiles
3. build a timetable from one or more GTFS feeds
4. run the server with the timetable directory and transfer graph directory

If you already have Valhalla tiles, the shortest path looks like this.

Build the transfer graph:

```bash
cargo run -p solari-export-graph -- \
  --valhalla_tiles data/valhalla_tiles \
  --output data/transfer_graph
```

This writes:

- `graph_metadata.db`
- `transfer_graph.bin`
- `transfer_node_index.bin`

Build the timetable from GTFS:

```bash
cargo run -p solari-cli -- build \
  --base_path data/timetable \
  --gtfs_path data/gtfs \
  --valhalla_tiles data/transfer_graph
```

Notes:

- `--gtfs_path` can point to a single GTFS zip or a directory of GTFS zips
- `--valhalla_tiles` here is currently the path passed into the timetable build step for transfer generation support
- `--num_threads` is optional if you want to speed up the build

Run the server:

```bash
cargo run -p solari-server -- \
  --base_path data/timetable \
  --valhalla_tile_path data/transfer_graph \
  --port 8000
```

Then test it:

```bash
curl -X POST http://localhost:8000/v1/plan \
  -H 'content-type: application/json' \
  --data @sample_request.json
```

Important detail: despite the flag name `--valhalla_tile_path`, the server is actually looking for the exported transfer graph files in that directory, not raw Valhalla tiles. If you omit the flag, the server assumes those files live in `base_path`.

### Getting Valhalla tiles

Solari assumes you already have Valhalla tiles available.

If you do not, you need to build or obtain them separately first. A practical setup is:

- raw Valhalla tiles in `data/valhalla_tiles/`
- exported Solari transfer graph in `data/transfer_graph/`
- Solari timetable in `data/timetable/`

We should eventually add a fuller end-to-end guide for acquiring or building Valhalla tiles, but the commands above are enough to get Solari running if you already have them.

## Architecture
- **RAPTOR Algorithm**: Implements all pruning rules from the original paper for optimal performance.
- **Memory Mapping**: Uses `memmap2` to load timetable data directly from disk, enabling fast access without RAM overhead.

## Roadmap
- **GTFS-RT Support** (priority order):
  1. Service alerts and closures
  2. Real-time delays
- **Performance Quantification**: Come up with better benchmarks against MOTIS and OpenTripPlanner.
- **rRAPTOR Implementation**: Long-term goal for multi-departure-time routing.
- **Documentation**: Ongoing work to finalize API response formats and provide detailed guides.

## Contributing
- Solari is in active development; contributions (documentation, testing, or features) are welcome.
- Check the repository's issue tracker for tasks, but note there are no formal contribution guidelines yet.

## Known Limitations
- **No Real-Time Updates**: Only static GTFS feeds supported currently.
- **API Stability**: The `/v1/plan` response format may evolve as documentation finalizes, but no compatibility breaking changes to the v1 endpoint after the initial release.

## When to Use Solari?
You may want to use this project if you need:
- Fast, lightweight routing for global-scale transit networks on modest hardware.
- A minimal API layer that integrates easily with modern web stacks (geocoding, map rendering handled externally).

Avoid if you require:
- Full-featured trip-planning like OpenTripPlanner's extensive customization or real-time capabilities.

## License
[Apache-2.0](LICENSE)
