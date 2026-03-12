use crate::timetable::in_memory::InMemoryTimetableBuilder;
use crate::timetable::mmap::MmapTimetable;
use anyhow::{bail, Result};
use chrono::NaiveDate;
use gtfs_structures::GtfsReader;
use rayon::prelude::*;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, error, info};

struct FeedProgressReporter {
    total: usize,
    start: Instant,
    last_log: std::sync::Mutex<Instant>,
}

impl FeedProgressReporter {
    fn new(total: usize) -> Self {
        let now = Instant::now();
        Self {
            total,
            start: now,
            last_log: std::sync::Mutex::new(now),
        }
    }

    fn maybe_log(&self, completed: usize) {
        let now = Instant::now();
        let mut last_log = self.last_log.lock().unwrap();
        if completed < self.total && now.duration_since(*last_log) < Duration::from_secs(10) {
            return;
        }
        let pct = if self.total == 0 {
            100.0
        } else {
            (completed as f64 * 100.0) / self.total as f64
        };
        let elapsed = self.start.elapsed();
        if completed > 0 && completed < self.total {
            let eta = Duration::from_secs_f64(
                elapsed.as_secs_f64() * (self.total - completed) as f64 / completed as f64,
            );
            info!(
                completed_feeds = completed,
                total_feeds = self.total,
                percent = pct,
                elapsed_seconds = elapsed.as_secs_f64(),
                eta_seconds = eta.as_secs_f64(),
                "prepared feed shards progress"
            );
        } else {
            info!(
                completed_feeds = completed,
                total_feeds = self.total,
                percent = pct,
                elapsed_seconds = elapsed.as_secs_f64(),
                "prepared feed shards progress"
            );
        }
        *last_log = now;
    }
}

fn shard_hash(path: &PathBuf) -> String {
    let mut hasher = DefaultHasher::new();
    hasher.write(path.to_str().unwrap().as_bytes());
    format!("{:x}", hasher.finish())
}

fn process_gtfs<'a>(
    path: &PathBuf,
    base_path: &PathBuf,
    start_date: Option<NaiveDate>,
    num_days: Option<u16>,
) -> Result<MmapTimetable<'a>, anyhow::Error> {
    let total_start = Instant::now();
    let read_start = Instant::now();
    let feed = if let Ok(feed) = GtfsReader::default().read_from_path(path.to_str().unwrap()) {
        feed
    } else {
        bail!(format!("Failed to load feed: {:?}", path));
    };
    let read_elapsed = read_start.elapsed();
    info!(path = ?path, seconds = read_elapsed.as_secs_f64(), "gtfs read complete");
    debug!("Processing feed: {:?}", path);

    let build_start = Instant::now();
    let in_memory_timetable_builder = InMemoryTimetableBuilder::new(&feed, start_date, num_days)?;
    let build_elapsed = build_start.elapsed();
    info!(path = ?path, seconds = build_elapsed.as_secs_f64(), "in-memory timetable build complete");

    let hash = shard_hash(path);

    let timetable_dir = base_path.join(hash);
    let complete_marker = timetable_dir.join("feed.complete");
    if complete_marker.exists() {
        return MmapTimetable::open(&timetable_dir).map_err(Into::into);
    }
    if timetable_dir.exists() {
        fs::remove_dir_all(&timetable_dir).unwrap();
    }
    fs::create_dir_all(&timetable_dir).unwrap();
    let mmap_start = Instant::now();
    let timetable = MmapTimetable::from_in_memory(&in_memory_timetable_builder, &timetable_dir)?;
    let mmap_elapsed = mmap_start.elapsed();
    fs::write(complete_marker, b"ok").unwrap();
    info!(path = ?path, seconds = mmap_elapsed.as_secs_f64(), "mmap timetable write complete");
    info!(path = ?path, seconds = total_start.elapsed().as_secs_f64(), "gtfs end-to-end processing complete");
    Ok(timetable)
}

pub async fn concat_timetables<'a>(
    paths: &[PathBuf],
    base_path: &PathBuf,
    valhalla_tile_path: &PathBuf,
) -> Result<MmapTimetable<'a>, anyhow::Error> {
    let paths = paths.to_vec();

    let timetables: Vec<MmapTimetable<'_>> = paths
        .par_iter()
        .filter_map(|path| MmapTimetable::open(path).ok())
        .collect();

    // Combine all timetables into one.
    let timetable = MmapTimetable::concatenate(&timetables, base_path, valhalla_tile_path).await;
    Ok(timetable)
}

pub async fn prepare_timetables_from_feeds<'a>(
    paths: &[PathBuf],
    base_path: &PathBuf,
    start_date: Option<NaiveDate>,
    num_days: Option<u16>,
) -> Result<Vec<PathBuf>, anyhow::Error> {
    fs::create_dir_all(base_path)?;
    let paths = paths.to_vec();

    let completed = AtomicUsize::new(0);
    let total = paths
        .iter()
        .filter(|path| path.extension().map(|ext| ext == "zip") == Some(true))
        .count();
    let progress = FeedProgressReporter::new(total);

    let mut shard_dirs: Vec<PathBuf> = paths
        .par_iter()
        .filter(|path| path.extension().map(|ext| ext == "zip") == Some(true))
        .filter_map(|path| {
            let shard_dir = base_path.join(shard_hash(path));
            let complete_marker = shard_dir.join("feed.complete");
            if complete_marker.exists() {
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                progress.maybe_log(done);
                return Some(shard_dir);
            }
            process_gtfs(&path, base_path, start_date, num_days)
                .map(|_| {
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    progress.maybe_log(done);
                    shard_dir
                })
                .map_err(|err| {
                    error!("Failed to process GTFS feed: {}", err);
                    err
                })
                .ok()
        })
        .collect();

    shard_dirs.sort();
    Ok(shard_dirs)
}

pub async fn timetable_from_feeds<'a>(
    paths: &[PathBuf],
    base_path: &PathBuf,
    valhalla_tile_path: &PathBuf,
    start_date: Option<NaiveDate>,
    num_days: Option<u16>,
) -> Result<MmapTimetable<'a>, anyhow::Error> {
    let shard_dirs = prepare_timetables_from_feeds(paths, base_path, start_date, num_days).await?;
    let timetable = concat_timetables(&shard_dirs, base_path, valhalla_tile_path).await?;
    Ok(timetable)
}
