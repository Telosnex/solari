use std::path::PathBuf;

use clap::Parser;
use solari_transfers::TransferGraphBuildArtifacts;
use serde_json::to_string_pretty;
use tracing::info;
use tracing_subscriber::FmtSubscriber;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    valhalla_tiles: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = false)]
    prepare_only: bool,
    #[arg(long, default_value_t = false)]
    reduction_stats_only: bool,
}

fn main() -> Result<(), anyhow::Error> {
    tracing::subscriber::set_global_default(FmtSubscriber::new())
        .expect("setting tracing default failed");

    let args = Args::parse();
    info!(
        valhalla_tiles = %args.valhalla_tiles.display(),
        output = %args.output.display(),
        "starting solari transfer graph export"
    );
    info!("preparing resumable pre-contraction build artifacts");
    let prepared = TransferGraphBuildArtifacts::prepare(&args.valhalla_tiles, &args.output)?;
    info!(
        compact_nodes = prepared.compact_node_count(),
        "pre-contraction seam ready"
    );
    if args.reduction_stats_only {
        let stats = prepared.analyze_reduction_potential()?;
        println!("{}", to_string_pretty(&stats)?);
        return Ok(());
    }
    if args.prepare_only {
        info!("prepare-only requested; leaving contraction for a later resumable run");
        return Ok(());
    }
    info!("starting contraction");
    prepared.contract_and_write_graph()?;
    info!("transfer graph export complete");
    Ok(())
}
