use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use geo::ClosestPoint;
use geo_types::{Coord, Line, LineString, Point};
use s2::latlng::LatLng;
use serde::Serialize;
use solari_geomath::EARTH_RADIUS_APPROX;
use solari_spatial::SphereIndexMmap;
use solari_transfers::{fast_paths::FastGraphStatic, TransferGraph, TransferGraphSearcher};
use time::OffsetDateTime;
use tracing::{debug, error, info, trace};

use crate::{
    api::{
        response::{ResponseStatus, SolariResponse},
        SolariItinerary, SolariLeg,
    },
    spatial::FAKE_WALK_SPEED_SECONDS_PER_METER,
    timetable::TripStopTime,
};

use crate::timetable::{Route, RouteStop, Stop, Time, Timetable, Trip};

pub struct Router<'a, T: Timetable<'a>> {
    timetable: T,
    transfer_graph: Arc<TransferGraph<FastGraphStatic<'a>, SphereIndexMmap<'a, usize>>>,
}

impl<'a, T: Timetable<'a>> Router<'a, T> {
    pub fn new(timetable: T, transfer_graph_path: PathBuf) -> Result<Router<'a, T>, anyhow::Error> {
        info!("Opening transfer graph metadata db.");
        let database = Arc::new(redb::Database::open(
            transfer_graph_path.join("graph_metadata.db"),
        )?);
        info!("Opening transfer graph.");
        let transfer_graph = Arc::new(
            TransferGraph::<FastGraphStatic, SphereIndexMmap<usize>>::read_from_dir(
                transfer_graph_path.clone(),
                Some(database),
            )?,
        );
        info!("Built router");
        Ok(Router {
            timetable,
            transfer_graph,
        })
    }

    pub fn nearest_stops(
        &'a self,
        location: LatLng,
        max_stops: Option<usize>,
        max_distance: Option<f64>,
    ) -> Vec<&'a Stop> {
        let mut stops: Vec<&'a Stop> = vec![];
        assert!(max_stops.is_some() || max_distance.is_some());
        for (count, (stop, dist_sq)) in self
            .timetable
            .nearest_stops(location.lat.deg(), location.lng.deg(), 100)
            .iter()
            .enumerate()
        {
            if let Some(max_stops) = max_stops {
                if count >= max_stops {
                    break;
                }
            }
            if let Some(max_distance) = max_distance {
                if *dist_sq > max_distance {
                    break;
                }
            }
            stops.push(self.timetable.stop(stop.id()));
        }
        stops
    }

    pub async fn route(
        &'a self,
        route_start_time: Time,
        start_location: LatLng,
        target_location: LatLng,
        max_distance_meters: Option<f64>,
        max_candidate_stops_each_side: Option<usize>,
        max_steps: Option<usize>,
        max_step_delta: Option<usize>,
    ) -> SolariResponse {
        self.route_inner(
            Some(route_start_time),
            None,
            start_location,
            target_location,
            max_distance_meters,
            max_candidate_stops_each_side,
            max_steps,
            max_step_delta,
        )
        .await
    }

    pub async fn route_arrive_by(
        &'a self,
        end_at: Time,
        start_location: LatLng,
        target_location: LatLng,
        max_distance_meters: Option<f64>,
        max_candidate_stops_each_side: Option<usize>,
        max_steps: Option<usize>,
        max_step_delta: Option<usize>,
    ) -> SolariResponse {
        self.route_inner(
            None,
            Some(end_at),
            start_location,
            target_location,
            max_distance_meters,
            max_candidate_stops_each_side,
            max_steps,
            max_step_delta,
        )
        .await
    }

    async fn route_inner(
        &'a self,
        start_at: Option<Time>,
        end_at: Option<Time>,
        start_location: LatLng,
        target_location: LatLng,
        max_distance_meters: Option<f64>,
        max_candidate_stops_each_side: Option<usize>,
        max_steps: Option<usize>,
        max_step_delta: Option<usize>,
    ) -> SolariResponse {
        let is_backward = end_at.is_some();

        let start_stops = self.nearest_stops(
            start_location,
            max_candidate_stops_each_side,
            max_distance_meters,
        );
        let target_stops = self.nearest_stops(
            target_location,
            max_candidate_stops_each_side,
            max_distance_meters,
        );

        info!(
            start_lat = start_location.lat.deg(),
            start_lng = start_location.lng.deg(),
            target_lat = target_location.lat.deg(),
            target_lng = target_location.lng.deg(),
            start_stops = start_stops.len(),
            target_stops = target_stops.len(),
            max_distance_meters = ?max_distance_meters,
            max_candidate_stops = ?max_candidate_stops_each_side,
            is_backward = is_backward,
            "route: nearest stops found"
        );
        if !start_stops.is_empty() {
            let first = start_stops[0];
            info!(
                stop_name = ?first.metadata(&self.timetable).name,
                stop_lat = first.location().lat.deg(),
                stop_lng = first.location().lng.deg(),
                "route: first start stop"
            );
        }
        if !target_stops.is_empty() {
            let first = target_stops[0];
            info!(
                stop_name = ?first.metadata(&self.timetable).name,
                stop_lat = first.location().lat.deg(),
                stop_lng = first.location().lng.deg(),
                "route: first target stop"
            );
        }

        if is_backward {
            // Reverse RAPTOR: targets are "sources", starts are "targets".
            // We initialize from target stops and propagate backward to
            // find the latest departure from start stops.
            let start_costs: Vec<(usize, u32)> = start_stops
                .iter()
                .map(|stop| {
                    (
                        stop.id(),
                        (FAKE_WALK_SPEED_SECONDS_PER_METER
                            * stop.location().distance(&start_location).rad()
                            * EARTH_RADIUS_APPROX) as u32,
                    )
                })
                .collect();

            let mut context = RouterContext {
                best_times_per_round: Vec::new(),
                marked_stops: Vec::new(),
                marked_routes: Vec::new(),
                timetable: &self.timetable,
                targets: start_costs.clone(),
                max_steps,
                max_step_delta,
                step_log: vec![InternalStep {
                    previous_step: 0usize,
                    from: InternalStepLocation::Location(LatLng::from_degrees(0.0, 0.0)),
                    to: InternalStepLocation::Location(LatLng::from_degrees(0.0, 0.0)),
                    route: None,
                    round: 0,
                    departure: Time::epoch(),
                    arrival: Time::epoch(),
                    trip: None,
                }],
            };
            context
                .init_backward(end_at.unwrap(), target_location, &target_stops)
                .await;
            context.route_backward().await;

            let rounds_used = context.best_times_per_round.len();
            let targets_reached = start_costs
                .iter()
                .filter(|(id, _)| {
                    context.best_times_per_round.iter().any(|round| round[*id].is_some())
                })
                .count();
            info!(
                rounds_used = rounds_used,
                targets_reached = targets_reached,
                total_targets = start_costs.len(),
                total_steps = context.step_log.len(),
                "route: backward RAPTOR complete"
            );

            let best_itineraries = self
                .pick_best_itineraries_backward(&context, &start_costs)
                .iter()
                .map(|itinerary| {
                    self.unwind_itinerary_backward(
                        &context,
                        itinerary,
                        end_at.unwrap(),
                        &start_costs,
                        start_location,
                        target_location,
                    )
                })
                .collect();

            SolariResponse {
                status: ResponseStatus::Ok,
                itineraries: best_itineraries,
            }
        } else {
            let target_costs: Vec<(usize, u32)> = target_stops
                .iter()
                .map(|stop| {
                    (
                        stop.id(),
                        (FAKE_WALK_SPEED_SECONDS_PER_METER
                            * stop.location().distance(&target_location).rad()
                            * EARTH_RADIUS_APPROX) as u32,
                    )
                })
                .collect();

            let route_start_time = start_at.unwrap();
            let mut context = RouterContext {
                best_times_per_round: Vec::new(),
                marked_stops: Vec::new(),
                marked_routes: Vec::new(),
                timetable: &self.timetable,
                targets: target_costs.clone(),
                max_steps,
                max_step_delta,
                step_log: vec![InternalStep {
                    previous_step: 0usize,
                    from: InternalStepLocation::Location(LatLng::from_degrees(0.0, 0.0)),
                    to: InternalStepLocation::Location(LatLng::from_degrees(0.0, 0.0)),
                    route: None,
                    round: 0,
                    departure: Time::epoch(),
                    arrival: Time::epoch(),
                    trip: None,
                }],
            };
            context
                .init(route_start_time, start_location, &start_stops)
                .await;
            context.route().await;

            let rounds_used = context.best_times_per_round.len();
            let targets_reached = target_costs
                .iter()
                .filter(|(id, _)| {
                    context.best_times_per_round.iter().any(|round| round[*id].is_some())
                })
                .count();
            info!(
                rounds_used = rounds_used,
                targets_reached = targets_reached,
                total_targets = target_costs.len(),
                total_steps = context.step_log.len(),
                "route: RAPTOR complete"
            );

            let best_itineraries = self
                .pick_best_itineraries(&context, &target_costs)
                .iter()
                .map(|itinerary| {
                    self.unwind_itinerary(
                        &context,
                        itinerary,
                        route_start_time,
                        &target_costs,
                        start_location,
                        target_location,
                    )
                })
                .collect();

            SolariResponse {
                status: ResponseStatus::Ok,
                itineraries: best_itineraries,
            }
        }
    }

    fn unwind_itinerary(
        &'a self,
        context: &RouterContext<'a, T>,
        itinerary: &InternalItinerary,
        route_start_time: Time,
        target_costs: &[(usize, u32)],
        start_location: LatLng,
        target_location: LatLng,
    ) -> SolariItinerary {
        let mut steps = vec![];
        let mut step_cursor = itinerary.last_step;
        {
            let step = &context.step_log[step_cursor];
            let from = if let InternalStepLocation::Stop(stop) = step.to {
                stop
            } else {
                panic!();
            };
            let to = if let InternalStepLocation::Stop(stop) = step.to {
                stop
            } else {
                panic!();
            };
            let from_location = from.location();
            let last_leg_cost = target_costs
                .iter()
                .find(|(target, _cost)| target == &to.id())
                .map(|(_target, cost)| *cost)
                .expect("Target cost not found");
            steps.push((
                Step::End(EndStep {
                    last_stop: from.metadata(&self.timetable).name.clone(),
                    last_stop_latlng: [from_location.lat.deg(), from_location.lng.deg()],
                    last_stop_departure_epoch_seconds: step.arrival.epoch_seconds() as u64,
                    end_latlng: [target_location.lat.deg(), target_location.lng.deg()],
                    end_epoch_seconds: (step.arrival.epoch_seconds() + last_leg_cost) as u64,
                }),
                step_cursor,
            ));
        }
        while context.step_log[step_cursor].previous_step != 0 {
            let step = &context.step_log[step_cursor];
            let to = if let InternalStepLocation::Stop(stop) = step.to {
                stop
            } else {
                panic!();
            };
            let from = if let InternalStepLocation::Stop(stop) = step.from {
                stop
            } else {
                panic!();
            };
            let to_location = to.location();
            let from_location = from.location();

            steps.push((
                if step.route.is_none() {
                    Step::Transfer(TransferStep {
                        from_stop: from.metadata(&self.timetable).name.clone(),
                        from_stop_latlng: [from_location.lat.deg(), from_location.lng.deg()],
                        to_stop: to.metadata(&self.timetable).name.clone(),
                        to_stop_latlng: [to_location.lat.deg(), to_location.lng.deg()],
                        departure_epoch_seconds: step.departure.epoch_seconds() as u64,
                        arrival_epoch_seconds: step.arrival.epoch_seconds() as u64,
                    })
                } else {
                    let to_location = to.location();
                    let from_location = from.location();

                    let shape = self.clip_shape(step);

                    Step::Trip(TripStep {
                        on_route: step
                            .trip
                            .unwrap()
                            .metadata(&self.timetable)
                            .route_name
                            .clone(),
                        agency: step
                            .trip
                            .unwrap()
                            .metadata(&self.timetable)
                            .agency_name
                            .clone(),
                        departure_stop: from.metadata(&self.timetable).name.clone(),
                        departure_stop_latlng: [from_location.lat.deg(), from_location.lng.deg()],
                        departure_epoch_seconds: step.departure.epoch_seconds() as u64,
                        arrival_stop: to.metadata(&self.timetable).name.clone(),
                        arrival_stop_latlng: [to_location.lat.deg(), to_location.lng.deg()],
                        arrival_epoch_seconds: step.arrival.epoch_seconds() as u64,
                        shape,
                    })
                },
                step_cursor,
            ));
            step_cursor = step.previous_step;
        }
        let end_time = if let Some((Step::End(end), _)) = steps.first() {
            end.end_epoch_seconds
        } else {
            panic!("First step is not a Begin step.");
        };
        let transfer_graph = self.transfer_graph.clone();
        let mut search_context = TransferGraphSearcher::new(transfer_graph);
        let legs = steps
            .iter()
            .rev()
            .filter_map(|(step, _)| match step {
                Step::Trip(trip) => Some(SolariLeg::Transit {
                    start_time: OffsetDateTime::from_unix_timestamp(
                        trip.departure_epoch_seconds as i64,
                    )
                    .expect("Invalid Unix timestamp"),
                    end_time: OffsetDateTime::from_unix_timestamp(
                        trip.arrival_epoch_seconds as i64,
                    )
                    .expect("Invalid Unix timestamp"),
                    start_location: crate::api::LatLng {
                        lat: trip.departure_stop_latlng[0],
                        lon: trip.departure_stop_latlng[1],
                        stop: trip.departure_stop.clone(),
                    },
                    end_location: crate::api::LatLng {
                        lat: trip.arrival_stop_latlng[0],
                        lon: trip.arrival_stop_latlng[1],
                        stop: trip.arrival_stop.clone(),
                    },
                    transit_route: trip.on_route.clone(),
                    transit_agency: trip.agency.clone(),
                    route_shape: trip.shape.clone(),
                }),
                Step::Transfer(transfer) => {
                    let from_coord = Coord {
                        y: transfer.from_stop_latlng[0],
                        x: transfer.from_stop_latlng[1],
                    };
                    let to_coord = Coord {
                        y: transfer.to_stop_latlng[0],
                        x: transfer.to_stop_latlng[1],
                    };
                    let transfer_shape = match self.transfer_graph.transfer_path(
                        &mut search_context,
                        &from_coord,
                        &to_coord,
                    ) {
                        Ok(transfer_path) => Some(transfer_path.shape),
                        Err(err) => {
                            error!(
                                "Failed to calculate transfer path: {}, step: {:?}",
                                err, transfer
                            );
                            None
                        }
                    };
                    Some(SolariLeg::Transfer {
                        start_time: OffsetDateTime::from_unix_timestamp(
                            transfer.departure_epoch_seconds as i64,
                        )
                        .expect("Invalid Unix timestamp"),
                        end_time: OffsetDateTime::from_unix_timestamp(
                            transfer.arrival_epoch_seconds as i64,
                        )
                        .expect("Invalid Unix timestamp"),
                        start_location: crate::api::LatLng {
                            lat: transfer.from_stop_latlng[0],
                            lon: transfer.from_stop_latlng[1],
                            stop: transfer.from_stop.clone(),
                        },
                        end_location: crate::api::LatLng {
                            lat: transfer.to_stop_latlng[0],
                            lon: transfer.to_stop_latlng[1],
                            stop: transfer.to_stop.clone(),
                        },
                        route_shape: transfer_shape,
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        SolariItinerary {
            start_location: crate::api::LatLng {
                lat: start_location.lat.deg(),
                lon: start_location.lng.deg(),
                stop: None,
            },
            end_location: crate::api::LatLng {
                lat: target_location.lat.deg(),
                lon: target_location.lng.deg(),
                stop: None,
            },
            start_time:
                OffsetDateTime::from_unix_timestamp(route_start_time.epoch_seconds() as i64)
                    .expect("Invalid Unix timestamp"),
            end_time: OffsetDateTime::from_unix_timestamp(end_time as i64)
                .expect("Invalid Unix timestamp"),
            legs,
        }
    }

    fn cost_scaling_final_transfer(
        &self,
        context: &RouterContext<'a, T>,
        itinerary: &InternalItinerary,
        scalar: f64,
    ) -> u32 {
        let last_step = &context.step_log[itinerary.last_step];
        if last_step.trip.is_none() {
            let last_step_duration =
                last_step.arrival.epoch_seconds() - last_step.departure.epoch_seconds();
            let scaled = last_step_duration as f64 * scalar;
            return last_step.departure.epoch_seconds() + scaled as u32;
        } else {
            return last_step.arrival.epoch_seconds();
        }
    }

    fn pick_best_itineraries(
        &self,
        context: &RouterContext<'a, T>,
        target_costs: &[(usize, u32)],
    ) -> Vec<InternalItinerary> {
        let best_round_count = if let Some(round_count) = context.fewest_rounds_to_target() {
            round_count
        } else {
            return Vec::new();
        };

        let mut itineraries = HashSet::new();

        let walking_scalars = [0.5, 1.0, 2.0];

        let mut best_arrival_per_scenario: Vec<Option<Time>> = vec![None; walking_scalars.len()];
        let max_round = match (context.max_step_delta, context.max_steps) {
            (None, None) => context.best_times_per_round.len(),
            (None, Some(transfers)) => context.best_times_per_round.len().min(transfers),
            (Some(delta), None) => context
                .best_times_per_round
                .len()
                .min(best_round_count + delta),
            (Some(delta), Some(transfers)) => context
                .best_times_per_round
                .len()
                .min(best_round_count + delta)
                .min(transfers),
        };
        for round in 0..max_round {
            for (walking_scalar_idx, walking_scalar) in walking_scalars.iter().enumerate() {
                if let Some((itinerary, _)) = target_costs
                    .iter()
                    .filter_map(|(target_id, cost)| {
                        context.best_times_per_round[round as usize][*target_id]
                            .as_ref()
                            .map(|it| (it, *cost as f64 * walking_scalar))
                    })
                    .filter(|(it, _)| context.step_log[it.last_step].route.is_some())
                    .min_by_key(|(it, cost)| {
                        self.cost_scaling_final_transfer(context, *it, *walking_scalar)
                            + *cost as u32
                    })
                {
                    if let Some(previous_best_time) =
                        &mut best_arrival_per_scenario[walking_scalar_idx]
                    {
                        if &itinerary.final_time < previous_best_time {
                            *previous_best_time = itinerary.final_time;
                            itineraries.insert(itinerary.clone());
                        }
                    } else {
                        best_arrival_per_scenario[walking_scalar_idx] = Some(itinerary.final_time);
                        itineraries.insert(itinerary.clone());
                    }
                }
            }
        }

        let mut itineraries: Vec<_> = itineraries.into_iter().collect();
        itineraries.sort_by(|a, b| {
            if a.final_time < b.final_time {
                Ordering::Less
            } else if a.final_time > b.final_time {
                Ordering::Greater
            } else if context.step_log[a.last_step].round < context.step_log[b.last_step].round {
                Ordering::Less
            } else if context.step_log[a.last_step].round > context.step_log[b.last_step].round {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        itineraries
    }

    /// Pick best itineraries for backward RAPTOR.
    /// In backward mode, final_time is latest departure (maximize).
    /// "targets" here are the START stops (where the user departs from).
    fn pick_best_itineraries_backward(
        &self,
        context: &RouterContext<'a, T>,
        start_costs: &[(usize, u32)],
    ) -> Vec<InternalItinerary> {
        let best_round_count = if let Some(round_count) = context.fewest_rounds_to_target() {
            round_count
        } else {
            return Vec::new();
        };

        let mut itineraries = HashSet::new();
        let walking_scalars = [0.5, 1.0, 2.0];
        // In backward mode, "best" = latest departure.
        let mut best_departure_per_scenario: Vec<Option<Time>> = vec![None; walking_scalars.len()];
        let max_round = match (context.max_step_delta, context.max_steps) {
            (None, None) => context.best_times_per_round.len(),
            (None, Some(transfers)) => context.best_times_per_round.len().min(transfers),
            (Some(delta), None) => context
                .best_times_per_round
                .len()
                .min(best_round_count + delta),
            (Some(delta), Some(transfers)) => context
                .best_times_per_round
                .len()
                .min(best_round_count + delta)
                .min(transfers),
        };
        for round in 0..max_round {
            for (ws_idx, _walking_scalar) in walking_scalars.iter().enumerate() {
                if let Some((itinerary, _)) = start_costs
                    .iter()
                    .filter_map(|(target_id, cost)| {
                        context.best_times_per_round[round][*target_id]
                            .as_ref()
                            .map(|it| (it, *cost))
                    })
                    .filter(|(it, _)| context.step_log[it.last_step].route.is_some())
                    .max_by_key(|(it, cost)| {
                        // Maximize departure time minus walk cost.
                        it.final_time.epoch_seconds().saturating_sub(*cost)
                    })
                {
                    if let Some(prev_best) = &mut best_departure_per_scenario[ws_idx] {
                        if &itinerary.final_time > prev_best {
                            *prev_best = itinerary.final_time;
                            itineraries.insert(itinerary.clone());
                        }
                    } else {
                        best_departure_per_scenario[ws_idx] = Some(itinerary.final_time);
                        itineraries.insert(itinerary.clone());
                    }
                }
            }
        }

        let mut itineraries: Vec<_> = itineraries.into_iter().collect();
        // Sort by latest departure first (best), then fewest rounds.
        itineraries.sort_by(|a, b| {
            b.final_time
                .cmp(&a.final_time)
                .then(context.step_log[a.last_step].round.cmp(&context.step_log[b.last_step].round))
        });
        itineraries
    }

    /// Unwind a backward RAPTOR itinerary into legs.
    /// The step log is reversed: it goes from target stops toward origin stops.
    /// We need to reverse the legs so they go origin → target.
    fn unwind_itinerary_backward(
        &'a self,
        context: &RouterContext<'a, T>,
        itinerary: &InternalItinerary,
        end_at: Time,
        start_costs: &[(usize, u32)],
        start_location: LatLng,
        target_location: LatLng,
    ) -> SolariItinerary {
        // Collect steps from the step log (target → origin order).
        let mut raw_steps = vec![];
        let mut step_cursor = itinerary.last_step;
        while step_cursor != 0 {
            raw_steps.push(&context.step_log[step_cursor]);
            step_cursor = context.step_log[step_cursor].previous_step;
        }
        // raw_steps is now [closest_to_origin, ..., closest_to_target]
        // because backward RAPTOR's step log chains from target toward origin.
        // Actually, the step log chains: each step's previous_step points
        // toward the target (where we initialized). So raw_steps goes
        // origin → target. We want that order for legs.

        let transfer_graph = self.transfer_graph.clone();
        let mut search_context = TransferGraphSearcher::new(transfer_graph);

        // Build legs. In backward RAPTOR, step.from is the alight stop
        // (closer to target) and step.to is the board stop (closer to origin).
        // So for a forward-ordered leg: board at step.to, alight at step.from.
        let mut legs: Vec<SolariLeg> = Vec::new();
        for step in &raw_steps {
            if step.route.is_some() {
                // Transit leg: board at step.to (origin side), alight at step.from (target side).
                let board_stop = if let InternalStepLocation::Stop(s) = step.to { s } else { continue; };
                let alight_stop = if let InternalStepLocation::Stop(s) = step.from { s } else { continue; };
                let board_loc = board_stop.location();
                let alight_loc = alight_stop.location();

                // Get the actual departure/arrival times from the trip.
                let trip = step.trip.unwrap();
                let route = step.route.unwrap();
                let route_stops = route.route_stops(&self.timetable);

                // Find board and alight stop sequences.
                let board_seq = route_stops.iter()
                    .find(|rs| rs.stop(&self.timetable).id() == board_stop.id())
                    .map(|rs| rs.stop_seq());
                let alight_seq = route_stops.iter()
                    .find(|rs| rs.stop(&self.timetable).id() == alight_stop.id())
                    .map(|rs| rs.stop_seq());

                let dep_time = board_seq
                    .map(|seq| trip.stop_times(&self.timetable)[seq].departure())
                    .unwrap_or(step.departure);
                let arr_time = alight_seq
                    .map(|seq| trip.stop_times(&self.timetable)[seq].arrival())
                    .unwrap_or(step.arrival);

                let shape = self.clip_shape_backward(step);

                legs.push(SolariLeg::Transit {
                    start_time: OffsetDateTime::from_unix_timestamp(dep_time.epoch_seconds() as i64)
                        .expect("Invalid Unix timestamp"),
                    end_time: OffsetDateTime::from_unix_timestamp(arr_time.epoch_seconds() as i64)
                        .expect("Invalid Unix timestamp"),
                    start_location: crate::api::LatLng {
                        lat: board_loc.lat.deg(),
                        lon: board_loc.lng.deg(),
                        stop: board_stop.metadata(&self.timetable).name.clone(),
                    },
                    end_location: crate::api::LatLng {
                        lat: alight_loc.lat.deg(),
                        lon: alight_loc.lng.deg(),
                        stop: alight_stop.metadata(&self.timetable).name.clone(),
                    },
                    transit_route: trip.metadata(&self.timetable).route_name.clone(),
                    transit_agency: trip.metadata(&self.timetable).agency_name.clone(),
                    route_shape: shape,
                });
            } else {
                // Transfer leg: walk from step.to (origin side) to step.from (target side).
                let from_stop = if let InternalStepLocation::Stop(s) = step.to { s } else { continue; };
                let to_stop = if let InternalStepLocation::Stop(s) = step.from { s } else { continue; };
                let from_loc = from_stop.location();
                let to_loc = to_stop.location();

                let from_coord = Coord { y: from_loc.lat.deg(), x: from_loc.lng.deg() };
                let to_coord = Coord { y: to_loc.lat.deg(), x: to_loc.lng.deg() };
                let transfer_shape = match self.transfer_graph.transfer_path(
                    &mut search_context,
                    &from_coord,
                    &to_coord,
                ) {
                    Ok(path) => Some(path.shape),
                    Err(err) => {
                        error!("Failed to calculate transfer path: {}", err);
                        None
                    }
                };

                legs.push(SolariLeg::Transfer {
                    start_time: OffsetDateTime::from_unix_timestamp(
                        step.departure.epoch_seconds() as i64,
                    )
                    .expect("Invalid Unix timestamp"),
                    end_time: OffsetDateTime::from_unix_timestamp(
                        step.arrival.epoch_seconds() as i64,
                    )
                    .expect("Invalid Unix timestamp"),
                    start_location: crate::api::LatLng {
                        lat: from_loc.lat.deg(),
                        lon: from_loc.lng.deg(),
                        stop: from_stop.metadata(&self.timetable).name.clone(),
                    },
                    end_location: crate::api::LatLng {
                        lat: to_loc.lat.deg(),
                        lon: to_loc.lng.deg(),
                        stop: to_stop.metadata(&self.timetable).name.clone(),
                    },
                    route_shape: transfer_shape,
                });
            }
        }

        // Compute overall start/end times.
        let journey_start = if let Some(first_leg) = legs.first() {
            match first_leg {
                SolariLeg::Transit { start_time, .. } => *start_time,
                SolariLeg::Transfer { start_time, .. } => *start_time,
            }
        } else {
            OffsetDateTime::from_unix_timestamp(end_at.epoch_seconds() as i64)
                .expect("Invalid Unix timestamp")
        };

        SolariItinerary {
            start_location: crate::api::LatLng {
                lat: start_location.lat.deg(),
                lon: start_location.lng.deg(),
                stop: None,
            },
            end_location: crate::api::LatLng {
                lat: target_location.lat.deg(),
                lon: target_location.lng.deg(),
                stop: None,
            },
            start_time: journey_start,
            end_time: OffsetDateTime::from_unix_timestamp(end_at.epoch_seconds() as i64)
                .expect("Invalid Unix timestamp"),
            legs,
        }
    }

    fn clip_shape(&'a self, step: &InternalStep) -> Option<String> {
        if let Some(route) = &step.route {
            if let Some(shape) = self.timetable.route_shape(route) {
                let departure_stop_distance = if let InternalStepLocation::Stop(stop) = step.from {
                    route
                        .route_stops(&self.timetable)
                        .iter()
                        .filter(|route_stop| route_stop.stop(&self.timetable).id() == stop.id())
                        .next()
                        .map(|route_stop| route_stop.distance_along_route())?
                } else {
                    return None;
                };
                let arrival_stop_distance = if let InternalStepLocation::Stop(stop) = step.to {
                    route
                        .route_stops(&self.timetable)
                        .iter()
                        .filter(|route_stop| route_stop.stop(&self.timetable).id() == stop.id())
                        .next()
                        .map(|route_stop| route_stop.distance_along_route())?
                } else {
                    return None;
                };
                let mut coords = shape
                    .iter()
                    .skip_while(|coord| {
                        coord
                            .distance_along_shape()
                            .map(|dist| dist.is_nan() || dist < departure_stop_distance)
                            .unwrap_or(true)
                    })
                    .take_while(|coord| {
                        coord
                            .distance_along_shape()
                            .map(|dist| dist.is_nan() || dist < arrival_stop_distance)
                            .unwrap_or(true)
                    })
                    .map(|coord| Coord {
                        x: coord.lon(),
                        y: coord.lat(),
                    })
                    .collect::<Vec<_>>();

                if coords.is_empty() {
                    let points: Vec<Coord> = shape
                        .iter()
                        .map(|coord| Coord {
                            x: coord.lon(),
                            y: coord.lat(),
                        })
                        .collect();
                    let start =
                        Point::new(step.from.latlng().lng.deg(), step.from.latlng().lat.deg());
                    let end = Point::new(step.to.latlng().lng.deg(), step.to.latlng().lat.deg());
                    if let (Some((start_idx, start_point)), Some((end_idx, end_point))) = (
                        Self::closest_point(&start, &points),
                        Self::closest_point(&end, &points),
                    ) {
                        coords = shape
                            .iter()
                            .skip(start_idx + 1)
                            .take(end_idx - start_idx)
                            .map(|coord| Coord {
                                x: coord.lon(),
                                y: coord.lat(),
                            })
                            .collect();
                        coords.insert(0, start_point.0);
                        coords.push(end_point.0);
                    }
                }

                let line_string = LineString::new(coords);
                return polyline::encode_coordinates(line_string, 5).ok();
            }
        };
        None
    }

    /// clip_shape but with from/to swapped for backward RAPTOR steps,
    /// where step.from is the alight stop and step.to is the board stop.
    fn clip_shape_backward(&'a self, step: &InternalStep) -> Option<String> {
        let flipped = InternalStep {
            from: step.to.clone(),
            to: step.from.clone(),
            ..*step
        };
        self.clip_shape(&flipped)
    }

    fn closest_point(target: &Point, points: &Vec<Coord>) -> Option<(usize, Point)> {
        let (idx, closest) = points
            .windows(2)
            .map(|window| Line::new(window[0], window[1]))
            .map(|line| line.closest_point(target))
            .enumerate()
            .reduce(|a, b| {
                let closest = a.1.best_of_two(&b.1, *target);
                if a.1 == closest {
                    a
                } else {
                    b
                }
            })?;
        let closest = match closest {
            geo::Closest::Intersection(point) => point,
            geo::Closest::SinglePoint(point) => point,
            geo::Closest::Indeterminate => return None,
        };
        return Some((idx, closest));
    }
}

#[derive(Debug, Clone)]
struct InternalStep<'a> {
    previous_step: usize,
    round: u32,
    from: InternalStepLocation<'a>,
    to: InternalStepLocation<'a>,
    route: Option<Route>,
    departure: Time,
    arrival: Time,
    trip: Option<Trip>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct InternalItinerary {
    last_step: usize,
    final_time: Time,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StopMark {
    Unmarked,
    Marked,
    MarkedForTransfersOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct BeginStep {
    pub begin_latlng: [f64; 2],
    pub begin_epoch_seconds: u64,
    pub first_stop: String,
    pub first_stop_latlng: [f64; 2],
    pub first_stop_arrival_epoch_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TripStep {
    pub on_route: Option<String>,
    pub agency: Option<String>,
    pub departure_stop: Option<String>,
    pub departure_stop_latlng: [f64; 2],
    pub departure_epoch_seconds: u64,
    pub arrival_stop: Option<String>,
    pub arrival_stop_latlng: [f64; 2],
    pub arrival_epoch_seconds: u64,
    pub shape: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferStep {
    pub from_stop: Option<String>,
    pub from_stop_latlng: [f64; 2],
    pub to_stop: Option<String>,
    pub to_stop_latlng: [f64; 2],
    pub departure_epoch_seconds: u64,
    pub arrival_epoch_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EndStep {
    pub last_stop: Option<String>,
    pub last_stop_latlng: [f64; 2],
    pub last_stop_departure_epoch_seconds: u64,
    pub end_latlng: [f64; 2],
    pub end_epoch_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub enum Step {
    Begin(BeginStep),
    Trip(TripStep),
    Transfer(TransferStep),
    End(EndStep),
}

pub struct RouterContext<'a, T: Timetable<'a>> {
    best_times_per_round: Vec<Vec<Option<InternalItinerary>>>,
    marked_stops: Vec<Vec<StopMark>>,
    marked_routes: Vec<RefCell<Vec<TripStopTime>>>,
    timetable: &'a T,
    targets: Vec<(usize, u32)>,
    max_steps: Option<usize>,
    max_step_delta: Option<usize>,
    step_log: Vec<InternalStep<'a>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InternalStepLocation<'a> {
    Stop(&'a Stop),
    Location(LatLng),
}

impl<'a> InternalStepLocation<'a> {
    pub fn latlng(&'a self) -> LatLng {
        match self {
            InternalStepLocation::Stop(stop) => stop.location(),
            InternalStepLocation::Location(latlng) => latlng.clone(),
        }
    }
}

impl<'a, 'b, T: Timetable<'a>> RouterContext<'a, T>
where
    'b: 'a,
{
    fn fewest_rounds_to_target(&self) -> Option<usize> {
        for (round, best_times) in self.best_times_per_round.iter().enumerate() {
            if self.targets.iter().any(|(id, _)| best_times[*id].is_some()) {
                return Some(round);
            }
        }
        None
    }

    fn maybe_update_arrival_time_and_route(
        &mut self,
        round: u32,
        from: &InternalStepLocation<'a>,
        departure_time: Time,
        to: &InternalStepLocation<'a>,
        arrival_time: Time,
        via: Option<Route>,
        on_trip: Option<Trip>,
        previous_step: usize,
    ) -> bool {
        let mut marked = false;
        let mut step_log_idx = None;
        if let InternalStepLocation::Stop(stop) = to {
            for best_times in &mut self.best_times_per_round.iter_mut().skip(round as usize) {
                let is_best = if let Some(previous_best) = &best_times[stop.id()] {
                    let fastest = &arrival_time < &previous_best.final_time;
                    let equal_and_shorter = {
                        &arrival_time == &previous_best.final_time
                            && round <= self.step_log[previous_best.last_step].round
                            && departure_time > self.step_log[previous_best.last_step].departure
                    };
                    if fastest {
                        true
                    } else if equal_and_shorter {
                        trace!("Equal and shorter, same round");
                        true
                    } else {
                        false
                    }
                } else {
                    true
                };
                if is_best {
                    let latest_step = InternalStep {
                        round,
                        from: from.clone(),
                        to: to.clone(),
                        route: via,
                        trip: on_trip,
                        departure: departure_time.clone(),
                        arrival: arrival_time.clone(),
                        previous_step,
                    };

                    if step_log_idx.is_none() {
                        if round > 0 && self.step_log[previous_step].round >= round {
                            error!("Rounds are not advancing in maybe_update_arrival_time_and_route: {}, {}, {}", round, self.step_log[previous_step].round, latest_step.route.is_some());
                        }
                        step_log_idx = Some(self.step_log.len());
                        self.step_log.push(latest_step);
                    }

                    best_times[stop.id()] = Some(InternalItinerary {
                        final_time: arrival_time.clone(),
                        last_step: step_log_idx.expect("Logic error: Step log index not updated"),
                    });

                    marked = true
                }
            }
            if marked {
                self.marked_stops[round as usize][stop.id()] = StopMark::Marked;
            }
        }
        marked
    }

    async fn init(&mut self, time: Time, start_location: LatLng, starts: &[&'a Stop]) {
        self.best_times_per_round
            .push(vec![None; self.timetable.stop_count()]);
        self.marked_stops
            .push(vec![StopMark::Unmarked; self.timetable.stop_count()]);
        self.marked_routes.push(RefCell::new(vec![
            TripStopTime::marked();
            self.timetable.routes().len()
        ]));

        let start_costs: HashMap<usize, u32> = starts
            .iter()
            .enumerate()
            .map(|(i, start)| {
                (
                    i,
                    (FAKE_WALK_SPEED_SECONDS_PER_METER
                        * start.location().distance(&start_location).rad()
                        * EARTH_RADIUS_APPROX) as u32,
                )
            })
            .collect();
        for (stop_option_index, stop) in starts.iter().enumerate() {
            if let Some(cost) = start_costs.get(&stop_option_index) {
                self.maybe_update_arrival_time_and_route(
                    0u32,
                    &InternalStepLocation::Location(start_location),
                    time.clone(),
                    &InternalStepLocation::Stop(stop),
                    time.clone().plus_seconds(*cost),
                    None,
                    None,
                    0,
                );
            }
        }
    }

    fn earliest_trip_from(&self, route_stop: &RouteStop, not_before: &Time) -> Option<Trip> {
        let trips = route_stop.route(self.timetable).route_trips(self.timetable);
        let position = match trips.binary_search_by_key(not_before, |trip| {
            trip.stop_times(self.timetable)[route_stop.stop_seq()].departure()
        }) {
            Ok(position) => position,
            Err(position) => position,
        };
        if position >= trips.len() {
            None
        } else {
            Some(trips[position])
        }
    }

    /// Find the latest trip that arrives at `route_stop` no later than `not_after`.
    fn latest_trip_arriving_by(&self, route_stop: &RouteStop, not_after: &Time) -> Option<Trip> {
        let trips = route_stop.route(self.timetable).route_trips(self.timetable);
        // Binary search for the last trip arriving <= not_after
        let position = match trips.binary_search_by_key(not_after, |trip| {
            trip.stop_times(self.timetable)[route_stop.stop_seq()].arrival()
        }) {
            Ok(position) => position,
            // Err(position) means not_after would be inserted at position,
            // so the last trip arriving <= not_after is at position - 1
            Err(position) => {
                if position == 0 {
                    return None;
                }
                position - 1
            }
        };
        Some(trips[position])
    }

    async fn do_round(&mut self, round: u32) -> bool {
        let mut marked_stops_total = 0usize;

        {
            while round as usize + 1 >= self.best_times_per_round.len() {
                self.best_times_per_round.push(
                    self.best_times_per_round
                        .last()
                        .cloned()
                        .expect("Logic error, best_times_per_round is empty."),
                );
                self.marked_stops
                    .push(vec![StopMark::Unmarked; self.timetable.stop_count()]);
                self.marked_routes.push(RefCell::new(vec![
                    TripStopTime::marked();
                    self.timetable.routes().len()
                ]));
            }
            // Mark routes based on stops that were marked in the previous round.
            {
                let mut new_marked_routes = self.marked_routes[round as usize].borrow_mut();
                for val in &mut (*new_marked_routes) {
                    *val = TripStopTime::marked();
                }
                for (stop_id, stop_marked) in
                    self.marked_stops[round as usize - 1].iter_mut().enumerate()
                {
                    if *stop_marked != StopMark::Marked {
                        continue;
                    }
                    *stop_marked = StopMark::MarkedForTransfersOnly;
                    Self::explore_routes_for_marked_stop(
                        self.timetable,
                        &mut *new_marked_routes,
                        self.timetable.stop(stop_id),
                        &self.best_times_per_round[round as usize - 1][stop_id]
                            .as_ref()
                            .unwrap()
                            .final_time,
                    );
                }
            }

            let mut marked_stops_count = 0usize;
            {
                let mut marked_routes: Vec<(usize, TripStopTime)> = self.marked_routes
                    [round as usize]
                    .borrow()
                    .iter()
                    .cloned()
                    .enumerate()
                    .collect();
                // Sort the marked routes for deterministic ordering.
                marked_routes.sort_by_key(|(_, trip_stop_time)| trip_stop_time.route_stop_seq);
                // Drop mutability.
                let marked_routes = marked_routes;

                for (route_id, departure) in marked_routes {
                    if departure.trip_index == usize::MAX {
                        continue;
                    }
                    let route = self.timetable.route(route_id);
                    let mut current_trip: Option<(Trip, RouteStop)> = None;
                    let mut found_first_stop = false;
                    let mut departure_stop_seq = 0usize;

                    for route_stop in route.route_stops(self.timetable) {
                        if route_stop.id() == departure.route_stop(self.timetable).id() {
                            found_first_stop = true;
                        }
                        if !found_first_stop {
                            departure_stop_seq += 1;
                            continue;
                        }
                        if let Some((current_trip, current_trip_start)) = &mut current_trip {
                            let departure_trip_stop_time =
                                &current_trip.stop_times(self.timetable)[departure_stop_seq];
                            let previous_step = if let Some(previous_step) = self
                                .best_times_per_round[round as usize - 1]
                                [departure.route_stop(self.timetable).id()]
                            .as_ref()
                            .map(|step| step.last_step.clone())
                            {
                                previous_step
                            } else {
                                error!(
                                    "No best time for stop {:?}",
                                    departure.route_stop(self.timetable)
                                );
                                continue;
                            };
                            if self.maybe_update_arrival_time_and_route(
                                round,
                                &InternalStepLocation::Stop(
                                    current_trip_start.stop(self.timetable),
                                ),
                                departure_trip_stop_time.departure(),
                                &InternalStepLocation::Stop(route_stop.stop(self.timetable)),
                                current_trip.stop_times(self.timetable)[route_stop.stop_seq()]
                                    .arrival(),
                                Some(current_trip.route(self.timetable)),
                                Some(current_trip.clone()),
                                previous_step,
                            ) {
                                marked_stops_count += 1;

                                if let Some(trip) = self.earliest_trip_from(
                                    departure.route_stop(self.timetable),
                                    &self.best_times_per_round[round as usize - 1]
                                        [departure.route_stop(self.timetable).id()]
                                    .as_ref()
                                    .unwrap()
                                    .final_time,
                                ) {
                                    if trip.stop_times(self.timetable)[route_stop.stop_seq()]
                                        .arrival()
                                        < self.best_times_per_round[round as usize - 1]
                                            [departure.route_stop(self.timetable).id()]
                                        .as_ref()
                                        .unwrap()
                                        .final_time
                                    {
                                        *current_trip = trip;
                                    }
                                }
                            }
                        }

                        if current_trip.is_none() {
                            current_trip = self
                                .earliest_trip_from(route_stop, &departure.arrival())
                                .map(|trip| (trip, route_stop.clone()));
                        }
                    }
                }
            }
            debug!("Marked {} new stops", marked_stops_count);
            marked_stops_total += marked_stops_count;

            if marked_stops_count == 0 {
                return false;
            }
        }

        let mut marked_transfers_count = 0usize;
        let mut total_transfers_count = 0usize;
        let marked_stops = self.marked_stops[round as usize].clone();
        for (stop_id, stop_marked) in marked_stops.iter().enumerate() {
            if *stop_marked == StopMark::Unmarked {
                continue;
            }
            let stop = self.timetable.stop(stop_id);

            for transfer in self.timetable.transfers_from(stop_id) {
                let transfer_to = transfer.to(self.timetable);
                let last_step = if let Some(last_step) = self.best_times_per_round[round as usize]
                    [stop.id()]
                .as_ref()
                .map(|transfer| transfer.last_step)
                .clone()
                {
                    last_step
                } else {
                    error!("No transfer for stop {:?}", stop);
                    continue;
                };
                // Don't transfer twice in a row.
                if self.step_log[last_step].route.is_none() {
                    continue;
                }
                let best_arrival_at_transfer_start = self.best_times_per_round[round as usize]
                    [stop.id()]
                .as_ref()
                .unwrap()
                .final_time;
                let arrival_at_transfer_end =
                    best_arrival_at_transfer_start.plus_seconds(transfer.time_seconds());
                total_transfers_count += 1;
                if self.maybe_update_arrival_time_and_route(
                    round + 1,
                    &InternalStepLocation::Stop(stop),
                    best_arrival_at_transfer_start,
                    &InternalStepLocation::Stop(transfer_to),
                    arrival_at_transfer_end,
                    None,
                    None,
                    last_step,
                ) {
                    marked_transfers_count += 1;
                }
            }
        }
        debug!(
            "Marked {} of {} transfers.",
            marked_transfers_count, total_transfers_count
        );

        marked_stops_total > 0 || marked_transfers_count > 0
    }

    fn explore_routes_for_marked_stop(
        timetable: &'a T,
        marked_routes: &mut [TripStopTime],
        marked_stop: &Stop,
        not_before: &Time,
    ) {
        for stop_route in marked_stop.stop_routes(timetable) {
            let route = stop_route.route(timetable);
            if marked_routes[route.id()].trip_index == usize::MAX {
                for trip in route.route_trips(timetable) {
                    let trip_stop_time = &trip.stop_times(timetable)[stop_route.stop_seq()];
                    if &trip_stop_time.departure() < &not_before {
                        continue;
                    }

                    // The clause after the && here is included for determinism. It specifies that we will prefer getting onto a vehicle later rather than earlier if we can do so at multiple locations.
                    if trip_stop_time.departure() < marked_routes[route.id()].departure()
                        || (trip_stop_time.departure() == marked_routes[route.id()].departure()
                            && trip_stop_time.route_stop_seq
                                > marked_routes[route.id()].route_stop_seq)
                    {
                        marked_routes[route.id()] = *trip_stop_time;
                        // Any trips after this one do not need to be examined.
                        break;
                    }
                }
            } else {
                for trip in route.route_trips(timetable)
                    [0..=(marked_routes[route.id()].trip_index - route.first_route_trip)]
                    .iter()
                    .rev()
                {
                    let trip_stop_time = &trip.stop_times(timetable)[stop_route.stop_seq()];
                    if &trip_stop_time.departure() < &not_before {
                        // We are iterating in reverse, so nothing "after" this (before, temporally) needs to be examined.
                        break;
                    }

                    // The clause after the && here is included for determinism. It specifies that we will prefer getting onto a vehicle later rather than earlier if we can do so at multiple locations.
                    if trip_stop_time.departure() < marked_routes[route.id()].departure()
                        || (trip_stop_time.departure() == marked_routes[route.id()].departure()
                            && trip_stop_time.route_stop_seq
                                > marked_routes[route.id()].route_stop_seq)
                    {
                        marked_routes[route.id()] = *trip_stop_time;
                        // We are iterating in reverse, so we can't break here.
                    }
                }
            }
        }
    }

    pub async fn route(&mut self) {
        let mut round = 1; // Zero is reserved for start costs.
        let mut marked_stops = true;
        while marked_stops {
            if let Some(max_steps) = self.max_steps {
                if round > max_steps {
                    break;
                }
            }
            if let (Some(max_step_delta), Some(best_rounds_to_target)) =
                (self.max_step_delta, self.fewest_rounds_to_target())
            {
                if round >= best_rounds_to_target + max_step_delta {
                    break;
                }
            }
            marked_stops = self.do_round(round as u32).await;
            round += 1;
        }
    }

    // ---------------------------------------------------------------
    // Reverse RAPTOR: given an arrival deadline (end_at), find the
    // latest departure. The algorithm is the symmetric dual of
    // forward RAPTOR:
    //   - Initialize from TARGET stops with end_at minus walk cost.
    //   - Each round scans routes in REVERSE stop order.
    //   - Uses latest_trip_arriving_by instead of earliest_trip_from.
    //   - "Better" = later time (maximize departure), so final_time
    //     in InternalItinerary represents latest known departure.
    //   - Transfers subtract walk time instead of adding it.
    // ---------------------------------------------------------------

    async fn init_backward(&mut self, end_at: Time, target_location: LatLng, targets: &[&'a Stop]) {
        self.best_times_per_round
            .push(vec![None; self.timetable.stop_count()]);
        self.marked_stops
            .push(vec![StopMark::Unmarked; self.timetable.stop_count()]);
        self.marked_routes.push(RefCell::new(vec![
            TripStopTime::marked();
            self.timetable.routes().len()
        ]));

        for stop in targets {
            let walk_cost = (FAKE_WALK_SPEED_SECONDS_PER_METER
                * stop.location().distance(&target_location).rad()
                * EARTH_RADIUS_APPROX) as u32;
            // Latest time we can be at this stop and still walk to destination by end_at.
            let latest_at_stop = end_at.minus_seconds(walk_cost);
            self.maybe_update_departure_time(
                0u32,
                &InternalStepLocation::Location(target_location),
                latest_at_stop.clone(),
                &InternalStepLocation::Stop(stop),
                latest_at_stop,
                None,
                None,
                0,
            );
        }
    }

    /// Reverse analog of maybe_update_arrival_time_and_route.
    /// "Better" = later departure time (maximize).
    fn maybe_update_departure_time(
        &mut self,
        round: u32,
        from: &InternalStepLocation<'a>,
        departure_time: Time,
        to: &InternalStepLocation<'a>,
        arrival_time: Time,
        via: Option<Route>,
        on_trip: Option<Trip>,
        previous_step: usize,
    ) -> bool {
        let mut marked = false;
        let mut step_log_idx = None;
        if let InternalStepLocation::Stop(stop) = to {
            for best_times in &mut self.best_times_per_round.iter_mut().skip(round as usize) {
                let is_best = if let Some(previous_best) = &best_times[stop.id()] {
                    // Later departure is better in reverse mode.
                    let later = &departure_time > &previous_best.final_time;
                    let equal_and_fewer_rounds = {
                        &departure_time == &previous_best.final_time
                            && round <= self.step_log[previous_best.last_step].round
                    };
                    later || equal_and_fewer_rounds
                } else {
                    true
                };
                if is_best {
                    let step = InternalStep {
                        round,
                        from: from.clone(),
                        to: to.clone(),
                        route: via,
                        trip: on_trip,
                        departure: departure_time.clone(),
                        arrival: arrival_time.clone(),
                        previous_step,
                    };

                    if step_log_idx.is_none() {
                        step_log_idx = Some(self.step_log.len());
                        self.step_log.push(step);
                    }

                    best_times[stop.id()] = Some(InternalItinerary {
                        final_time: departure_time.clone(),
                        last_step: step_log_idx.expect("Logic error"),
                    });

                    marked = true;
                }
            }
            if marked {
                self.marked_stops[round as usize][stop.id()] = StopMark::Marked;
            }
        }
        marked
    }

    async fn do_round_backward(&mut self, round: u32) -> bool {
        let mut marked_stops_total = 0usize;

        {
            while round as usize + 1 >= self.best_times_per_round.len() {
                self.best_times_per_round.push(
                    self.best_times_per_round
                        .last()
                        .cloned()
                        .expect("Logic error, best_times_per_round is empty."),
                );
                self.marked_stops
                    .push(vec![StopMark::Unmarked; self.timetable.stop_count()]);
                self.marked_routes.push(RefCell::new(vec![
                    TripStopTime::marked();
                    self.timetable.routes().len()
                ]));
            }

            // Mark routes based on stops marked in previous round.
            {
                let mut new_marked_routes = self.marked_routes[round as usize].borrow_mut();
                for val in &mut (*new_marked_routes) {
                    *val = TripStopTime::marked();
                }
                for (stop_id, stop_marked) in
                    self.marked_stops[round as usize - 1].iter_mut().enumerate()
                {
                    if *stop_marked != StopMark::Marked {
                        continue;
                    }
                    *stop_marked = StopMark::MarkedForTransfersOnly;
                    // For backward: find latest trips arriving at this stop
                    // no later than the best known time.
                    Self::explore_routes_for_marked_stop_backward(
                        self.timetable,
                        &mut *new_marked_routes,
                        self.timetable.stop(stop_id),
                        &self.best_times_per_round[round as usize - 1][stop_id]
                            .as_ref()
                            .unwrap()
                            .final_time,
                    );
                }
            }

            let mut marked_stops_count = 0usize;
            {
                let mut marked_routes: Vec<(usize, TripStopTime)> = self.marked_routes
                    [round as usize]
                    .borrow()
                    .iter()
                    .cloned()
                    .enumerate()
                    .collect();
                marked_routes.sort_by_key(|(_, trip_stop_time)| trip_stop_time.route_stop_seq);
                let marked_routes = marked_routes;

                for (route_id, arrival_mark) in marked_routes {
                    if arrival_mark.trip_index == usize::MAX {
                        continue;
                    }
                    let route = self.timetable.route(route_id);
                    let route_stops = route.route_stops(self.timetable);
                    let arrival_route_stop = arrival_mark.route_stop(self.timetable);
                    let mut current_trip: Option<(Trip, RouteStop)> = None;
                    let mut found_marked_stop = false;

                    // Scan route stops in REVERSE order.
                    // In forward RAPTOR we scan forward to find where we can
                    // ride TO. In reverse we scan backward to find where we
                    // can ride FROM (i.e. board earlier along the route).
                    for route_stop in route_stops.iter().rev() {
                        if route_stop.id() == arrival_route_stop.id() {
                            found_marked_stop = true;
                        }
                        if !found_marked_stop {
                            continue;
                        }

                        if let Some((trip, boarded_at)) = &mut current_trip {
                            // We're on `trip`, which we know arrives at `boarded_at`
                            // by the deadline. Check the departure time at this
                            // earlier stop along the route.
                            let dep_at_this_stop =
                                trip.stop_times(self.timetable)[route_stop.stop_seq()]
                                    .departure();

                            let previous_step = if let Some(ps) = self.best_times_per_round
                                [round as usize - 1][boarded_at.stop(self.timetable).id()]
                            .as_ref()
                            .map(|s| s.last_step)
                            {
                                ps
                            } else {
                                continue;
                            };

                            // Record: depart from route_stop on this trip,
                            // arrive at boarded_at. In reverse RAPTOR the
                            // "from" in the step log is where we alight (the
                            // stop closer to the target) and "to" is where we
                            // board (the stop closer to the origin).
                            if self.maybe_update_departure_time(
                                round,
                                &InternalStepLocation::Stop(
                                    boarded_at.stop(self.timetable),
                                ),
                                dep_at_this_stop.clone(),
                                &InternalStepLocation::Stop(route_stop.stop(self.timetable)),
                                dep_at_this_stop,
                                Some(trip.route(self.timetable)),
                                Some(trip.clone()),
                                previous_step,
                            ) {
                                marked_stops_count += 1;
                            }

                            // Can we catch a later trip at this stop?
                            // (Symmetric to forward's "can we catch an earlier trip?")
                            if let Some(later_trip) =
                                self.latest_trip_arriving_by(route_stop, &self.best_times_per_round
                                    [round as usize - 1][route_stop.stop(self.timetable).id()]
                                    .as_ref()
                                    .map(|s| s.final_time)
                                    .unwrap_or(dep_at_this_stop.clone()))
                            {
                                let later_dep = later_trip.stop_times(self.timetable)
                                    [route_stop.stop_seq()]
                                    .departure();
                                if later_dep > dep_at_this_stop {
                                    *trip = later_trip;
                                    *boarded_at = route_stop.clone();
                                }
                            }
                        }

                        if current_trip.is_none() {
                            // Find latest trip arriving at this stop by the deadline.
                            if let Some(best) = &self.best_times_per_round[round as usize - 1]
                                [route_stop.stop(self.timetable).id()]
                            {
                                current_trip = self
                                    .latest_trip_arriving_by(route_stop, &best.final_time)
                                    .map(|trip| (trip, route_stop.clone()));
                            }
                        }
                    }
                }
            }
            debug!("Backward: Marked {} new stops", marked_stops_count);
            marked_stops_total += marked_stops_count;

            if marked_stops_count == 0 {
                return false;
            }
        }

        // Backward transfers: from each marked stop, walk backward in time.
        let mut marked_transfers_count = 0usize;
        let marked_stops = self.marked_stops[round as usize].clone();
        for (stop_id, stop_marked) in marked_stops.iter().enumerate() {
            if *stop_marked == StopMark::Unmarked {
                continue;
            }
            let stop = self.timetable.stop(stop_id);

            for transfer in self.timetable.transfers_from(stop_id) {
                let transfer_to = transfer.to(self.timetable);
                let last_step = if let Some(ls) = self.best_times_per_round[round as usize]
                    [stop.id()]
                .as_ref()
                .map(|t| t.last_step)
                {
                    ls
                } else {
                    continue;
                };
                // Don't transfer twice in a row.
                if self.step_log[last_step].route.is_none() {
                    continue;
                }
                let best_dep_at_stop = self.best_times_per_round[round as usize][stop.id()]
                    .as_ref()
                    .unwrap()
                    .final_time;
                // Going backward: must arrive at transfer origin earlier.
                let dep_at_transfer_dest = best_dep_at_stop.minus_seconds(transfer.time_seconds());
                if self.maybe_update_departure_time(
                    round + 1,
                    &InternalStepLocation::Stop(stop),
                    dep_at_transfer_dest.clone(),
                    &InternalStepLocation::Stop(transfer_to),
                    dep_at_transfer_dest,
                    None,
                    None,
                    last_step,
                ) {
                    marked_transfers_count += 1;
                }
            }
        }
        debug!(
            "Backward: Marked {} transfers.",
            marked_transfers_count
        );

        marked_stops_total > 0 || marked_transfers_count > 0
    }

    /// For backward search: find routes serving a marked stop where a trip
    /// arrives no later than `not_after`. Symmetric to explore_routes_for_marked_stop.
    fn explore_routes_for_marked_stop_backward(
        timetable: &'a T,
        marked_routes: &mut [TripStopTime],
        marked_stop: &Stop,
        not_after: &Time,
    ) {
        for stop_route in marked_stop.stop_routes(timetable) {
            let route = stop_route.route(timetable);
            // Iterate trips from latest to earliest to find the latest
            // trip arriving at this stop <= not_after.
            for trip in route.route_trips(timetable).iter().rev() {
                let tst = &trip.stop_times(timetable)[stop_route.stop_seq()];
                if &tst.arrival() > not_after {
                    continue;
                }
                // This is the latest trip arriving <= not_after.
                // In backward mode, we want the LATEST marked stop along
                // the route (closest to end), symmetric to forward wanting
                // the earliest marked stop.
                if marked_routes[route.id()].trip_index == usize::MAX
                    || tst.arrival() > marked_routes[route.id()].arrival()
                    || (tst.arrival() == marked_routes[route.id()].arrival()
                        && tst.route_stop_seq < marked_routes[route.id()].route_stop_seq)
                {
                    marked_routes[route.id()] = *tst;
                }
                break;
            }
        }
    }

    pub async fn route_backward(&mut self) {
        let mut round = 1;
        let mut marked_stops = true;
        while marked_stops {
            if let Some(max_steps) = self.max_steps {
                if round > max_steps {
                    break;
                }
            }
            if let (Some(max_step_delta), Some(best_rounds_to_target)) =
                (self.max_step_delta, self.fewest_rounds_to_target())
            {
                if round >= best_rounds_to_target + max_step_delta {
                    break;
                }
            }
            marked_stops = self.do_round_backward(round as u32).await;
            round += 1;
        }
    }
}
