use gtfs_structures::GtfsReader;
use std::env;
use std::fs;
use std::path::PathBuf;

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

#[test]
fn parse_local_gtfs_corpus_from_env() {
    let Some(dir) = env::var_os("SOLARI_GTFS_FIXTURE_DIR") else {
        eprintln!("skipping: SOLARI_GTFS_FIXTURE_DIR not set");
        return;
    };

    let dir = PathBuf::from(dir);
    let mut all_paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().map(|ext| ext == "zip") == Some(true))
        .collect();
    all_paths.sort();

    assert!(!all_paths.is_empty(), "no .zip files found in {:?}", dir);

    let shard_count = env_usize("SOLARI_GTFS_SHARD_COUNT", 1);
    let shard_index = env_usize("SOLARI_GTFS_SHARD_INDEX", 0);
    assert!(shard_count > 0, "SOLARI_GTFS_SHARD_COUNT must be > 0");
    assert!(shard_index < shard_count, "SOLARI_GTFS_SHARD_INDEX must be < SOLARI_GTFS_SHARD_COUNT");

    let paths: Vec<PathBuf> = all_paths
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % shard_count == shard_index)
        .map(|(_, p)| p)
        .collect();

    assert!(!paths.is_empty(), "no files in shard {}/{}", shard_index, shard_count);

    let mut ok = 0usize;
    let mut failed = Vec::new();

    for (n, path) in paths.iter().enumerate() {
        let feed_name = path.file_name().unwrap().to_string_lossy();
        match GtfsReader::default().read_from_path(path.to_str().unwrap()) {
            Ok(_) => {
                ok += 1;
                eprintln!("[{}/{}] {}/{} ok {}", shard_index, shard_count, n + 1, paths.len(), feed_name);
            }
            Err(err) => {
                failed.push((path.clone(), err.to_string()));
                eprintln!("[{}/{}] {}/{} fail {} :: {}", shard_index, shard_count, n + 1, paths.len(), feed_name, err);
            }
        }
    }

    eprintln!("shard [{}/{}] parsed {} / {} feeds successfully", shard_index, shard_count, ok, paths.len());
    if !failed.is_empty() {
        eprintln!("failed feeds: {}", failed.len());
        for (path, err) in failed.iter().take(50) {
            eprintln!("{} :: {}", path.display(), err);
        }
    }

    assert!(ok > 0, "no GTFS feeds parsed successfully out of {}", paths.len());
}
