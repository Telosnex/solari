use gtfs_structures::GtfsReader;
use std::env;
use std::path::Path;
use std::time::Instant;

#[test]
fn parse_single_gtfs_from_env() {
    let path = env::var("SOLARI_GTFS_PARSE_PATH").expect("SOLARI_GTFS_PARSE_PATH not set");
    let path_kind = if Path::new(&path).is_dir() { "directory" } else { "file" };
    eprintln!("starting parse from {}: {}", path_kind, path);
    let start = Instant::now();
    match GtfsReader::default().read_from_path(&path) {
        Ok(feed) => {
            eprintln!(
                "parse ok in {:.3}s stops={} trips={} routes={} agencies={}",
                start.elapsed().as_secs_f64(),
                feed.stops.len(),
                feed.trips.len(),
                feed.routes.len(),
                feed.agencies.len()
            );
        }
        Err(err) => {
            eprintln!("parse failed in {:.3}s :: {}", start.elapsed().as_secs_f64(), err);
            panic!("parse failed");
        }
    }
}
