use std::path::PathBuf;

use clap::Parser;
use rocket::{State, serde::json::Json};
use s2::latlng::LatLng;
use serde::Serialize;
use solari::{
    api::{request::SolariRequest, response::SolariResponse},
    route::Router,
    timetable::{Time, Timetable, mmap::MmapTimetable},
};
use time::OffsetDateTime;
use tracing_subscriber::FmtSubscriber;

#[macro_use]
extern crate rocket;

#[post("/v1/plan", data = "<request>")]
async fn plan(
    request: Json<SolariRequest>,
    router: &State<Router<'_, MmapTimetable<'_>>>,
) -> Json<SolariResponse> {
    let from = LatLng::from_degrees(request.0.from.lat, request.0.from.lon);
    let to = LatLng::from_degrees(request.0.to.lat, request.0.to.lon);

    let max_transfers = usize::min(5, request.0.max_transfers.0);

    let response = if let Some(end_at) = request.0.end_at {
        router
            .route_arrive_by(
                Time::from_epoch_seconds(end_at.unix_timestamp() as u32),
                from,
                to,
                Some(1500f64),
                Some(1000),
                Some(max_transfers),
                Some(2),
            )
            .await
    } else {
        let start_at = request.0.start_at
            .unwrap_or_else(|| OffsetDateTime::now_utc());
        router
            .route(
                Time::from_epoch_seconds(start_at.unix_timestamp() as u32),
                from,
                to,
                Some(1500f64),
                Some(1000),
                Some(max_transfers),
                Some(2),
            )
            .await
    };

    Json(response)
}

#[derive(Debug, Serialize)]
struct DebugStop {
    id: usize,
    name: String,
    lat: f64,
    lon: f64,
    transfer_count: usize,
    transfers_to: Vec<DebugTransfer>,
}

#[derive(Debug, Serialize)]
struct DebugTransfer {
    to_stop_id: usize,
    to_stop_name: String,
    to_lat: f64,
    to_lon: f64,
    duration_seconds: u32,
}

#[get("/debug/stops?<lat>&<lon>&<radius>&<max_stops>")]
fn debug_stops(
    lat: f64,
    lon: f64,
    radius: Option<f64>,
    max_stops: Option<usize>,
    router: &State<Router<'_, MmapTimetable<'_>>>,
) -> Json<Vec<DebugStop>> {
    let location = LatLng::from_degrees(lat, lon);
    let nearby = router.nearest_stops(
        location,
        Some(max_stops.unwrap_or(100)),
        radius,
    );
    let timetable = router.timetable();
    let mut stops = Vec::new();
    for stop in &nearby {
        let stop_id = stop.id();
        let transfers = timetable.transfers_from(stop_id);
        let transfers_to: Vec<DebugTransfer> = transfers.iter().map(|t| {
            let to_stop = t.to(timetable);
            let to_loc = to_stop.location();
            DebugTransfer {
                to_stop_id: to_stop.id(),
                to_stop_name: to_stop.metadata(timetable).name.clone().unwrap_or_default(),
                to_lat: to_loc.lat.deg(),
                to_lon: to_loc.lng.deg(),
                duration_seconds: t.time_seconds(),
            }
        }).collect();
        let loc = stop.location();
        stops.push(DebugStop {
            id: stop_id,
            name: stop.metadata(timetable).name.clone().unwrap_or_default(),
            lat: loc.lat.deg(),
            lon: loc.lng.deg(),
            transfer_count: transfers.len(),
            transfers_to,
        });
    }
    Json(stops)
}

#[derive(Parser)]
struct ServeArgs {
    #[arg(long)]
    base_path: PathBuf,
    #[arg(long)]
    valhalla_tile_path: Option<PathBuf>,
    #[arg(short, long)]
    port: Option<u16>,
}

#[launch]
fn rocket() -> _ {
    tracing::subscriber::set_global_default(FmtSubscriber::new())
        .expect("setting tracing default failed");

    let args = ServeArgs::parse();
    let router = Router::new(
        MmapTimetable::open(&args.base_path).expect("Failed to open timetable"),
        args.valhalla_tile_path.unwrap_or(args.base_path),
    )
    .expect("Failed to build router");

    rocket::build()
        .manage(router)
        .configure(rocket::Config::figment().merge(("port", args.port.unwrap_or(8000))).merge(("address", "0.0.0.0")))
        .mount("/", routes![plan, debug_stops])
}
