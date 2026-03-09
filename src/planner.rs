// high-level trip planner
// coordinates walking, raptor routing, and response formatting
// to match the wheels router web api

use crate::data::{TransitData, NOT_SET};
use crate::raptor::{self, RaptorJourney, RaptorQuery};
use crate::types::*;
use crate::walker;

const DEFAULT_MAX_WALK: u32 = 1000; // meters
const DEFAULT_MAX_TRANSFERS: u32 = 3;
const DEFAULT_MAX_RESULTS: usize = 15;

// convert gtfs route_type to a mode string
fn route_type_to_mode(rt: u8) -> &'static str {
    match rt {
        0 => "tram",
        1 => "subway",
        2 => "rail",
        3 => "bus",
        4 => "ferry",
        5 => "cable_tram",
        6 => "gondola",
        7 => "funicular",
        11 => "trolleybus",
        12 => "monorail",
        _ => "bus",
    }
}

// format a u32 color to hex string
fn color_to_hex(c: u32) -> String {
    format!("{:06x}", c & 0xFFFFFF)
}

// convert seconds since midnight + a base date string to an iso8601 timestamp
fn secs_to_iso(base_date: &str, secs: u32) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    // handle times past midnight
    if h >= 24 {
        // simplified: just wrap around
        format!("{}T{:02}:{:02}:{:02}Z", base_date, h - 24, m, s)
    } else {
        format!("{}T{:02}:{:02}:{:02}Z", base_date, h, m, s)
    }
}

pub struct PlanRequest {
    pub origin_lat: f64,
    pub origin_lon: f64,
    pub dest_lat: f64,
    pub dest_lon: f64,
    pub departure_time: u32, // seconds since midnight
    pub service_day: u16,    // days since epoch
    pub date_str: String,    // yyyy-mm-dd for output formatting
    pub max_transfers: Option<u32>,
    pub max_results: Option<usize>,
    pub max_walk_distance: Option<u32>,
    pub walking_speed: Option<String>,
}

pub fn plan(data: &TransitData, req: &PlanRequest) -> PlanResponse {
    let max_walk = req.max_walk_distance.unwrap_or(DEFAULT_MAX_WALK);
    let max_transfers = req.max_transfers.unwrap_or(DEFAULT_MAX_TRANSFERS);
    let max_results = req.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
    let walk_speed = parse_walk_speed(req.walking_speed.as_deref());

    // find stops reachable from origin by walking
    let origin_stops = walker::reachable_stops_from_coord(
        &data.walk_graph,
        &data.stops,
        req.origin_lat as f32,
        req.origin_lon as f32,
        max_walk,
        walk_speed,
    );

    // find stops reachable from destination by walking
    let dest_stops = walker::reachable_stops_from_coord(
        &data.walk_graph,
        &data.stops,
        req.dest_lat as f32,
        req.dest_lon as f32,
        max_walk,
        walk_speed,
    );

    if origin_stops.is_empty() || dest_stops.is_empty() {
        return PlanResponse { plans: vec![] };
    }

    // run raptor
    let query = RaptorQuery {
        origin_stops: origin_stops.iter().map(|&(s, w, _)| (s, w)).collect(),
        dest_stops: dest_stops.iter().map(|&(s, w, _)| (s, w)).collect(),
        departure_time: req.departure_time,
        service_day: req.service_day,
        max_transfers,
        max_results,
    };

    let journeys = raptor::run(data, &query);

    // convert to api response
    let plans = journeys
        .iter()
        .filter_map(|j| journey_to_plan(data, j, req, &origin_stops, &dest_stops))
        .collect();

    PlanResponse { plans }
}

fn journey_to_plan(
    data: &TransitData,
    journey: &RaptorJourney,
    req: &PlanRequest,
    origin_stops: &[(u32, u32, u32)],
    dest_stops: &[(u32, u32, u32)],
) -> Option<Plan> {
    let mut legs = Vec::new();
    let date = &req.date_str;

    // access walk leg
    if journey.access_walk_secs > 0 {
        let stop = &data.stops[journey.access_stop_idx as usize];
        let access_dist = origin_stops
            .iter()
            .find(|(s, _, _)| *s == journey.access_stop_idx)
            .map(|(_, _, d)| *d)
            .unwrap_or(0);

        legs.push(Leg::Walk(WalkLeg {
            walk_type: Some("station_access".to_string()),
            from: Some(Location {
                location: LatLon {
                    lat: req.origin_lat,
                    lon: req.origin_lon,
                },
                address: Some("START".to_string()),
                id: None,
                stop_id: None,
                entrance: None,
                platform: None,
            }),
            to: Some(Location {
                location: LatLon {
                    lat: stop.lat as f64,
                    lon: stop.lon as f64,
                },
                address: Some(stop.name.clone()),
                id: Some(stop.id.clone()),
                stop_id: Some(stop.id.clone()),
                entrance: None,
                platform: if stop.platform_code.is_empty() {
                    None
                } else {
                    Some(stop.platform_code.clone())
                },
            }),
            duration_seconds: journey.access_walk_secs,
            distance_meters: Some(access_dist),
            polyline: None,
        }));
    }

    // transit legs
    for (i, rleg) in journey.legs.iter().enumerate() {
        let route = &data.routes[rleg.route_idx as usize];
        let trip_idx = route.trip_idxs[rleg.trip_num as usize];
        let trip = &data.trips[trip_idx as usize];
        let agency = &data.agencies[route.agency_idx as usize];

        let board_stop_idx = route.stop_idxs[rleg.board_stop_pos as usize];
        let alight_stop_idx = route.stop_idxs[rleg.alight_stop_pos as usize];
        let board_stop = &data.stops[board_stop_idx as usize];
        let alight_stop = &data.stops[alight_stop_idx as usize];

        // build intermediate stops
        let mut transit_stops = Vec::new();
        for pos in rleg.board_stop_pos..=rleg.alight_stop_pos {
            let sidx = route.stop_idxs[pos as usize];
            let s = &data.stops[sidx as usize];
            let arr = route.arrival_at(&data.stop_times, rleg.trip_num, pos as usize);
            let dep = route.departure_at(&data.stop_times, rleg.trip_num, pos as usize);

            let board_dep = rleg.board_time;
            let arr_offset = if arr != NOT_SET && arr >= board_dep {
                Some(((arr - board_dep) / 60) as i32)
            } else {
                None
            };
            let dep_offset = if dep != NOT_SET && dep >= board_dep {
                Some(((dep - board_dep) / 60) as i32)
            } else {
                None
            };

            transit_stops.push(StopInfo {
                location: LatLon {
                    lat: s.lat as f64,
                    lon: s.lon as f64,
                },
                id: Some(s.id.clone()),
                stop_id: Some(s.id.clone()),
                stop_name: Some(s.name.clone()),
                platform: if s.platform_code.is_empty() {
                    None
                } else {
                    Some(s.platform_code.clone())
                },
                arrival_offset_minutes: if pos == rleg.board_stop_pos {
                    None
                } else {
                    arr_offset
                },
                departure_offset_minutes: if pos == rleg.alight_stop_pos {
                    None
                } else {
                    dep_offset
                },
            });
        }

        let from_info = transit_stops.first().cloned().unwrap();
        let to_info = transit_stops.last().cloned().unwrap();
        let duration = rleg.alight_time.saturating_sub(rleg.board_time);

        let route_option = RouteOption {
            route_id: route.id.clone(),
            trip_id: Some(trip.id.clone()),
            route_name: if route.short_name.is_empty() {
                route.long_name.clone()
            } else {
                route.short_name.clone()
            },
            route_long_name: if route.long_name.is_empty() {
                None
            } else {
                Some(route.long_name.clone())
            },
            route_short_name: if route.short_name.is_empty() {
                None
            } else {
                Some(route.short_name.clone())
            },
            headsign: if trip.headsign.is_empty() {
                None
            } else {
                Some(trip.headsign.clone())
            },
            agency: AgencyInfo {
                id: agency.id.clone(),
                name: agency.name.clone(),
                url: if agency.url.is_empty() {
                    None
                } else {
                    Some(agency.url.clone())
                },
            },
            mode: route_type_to_mode(route.route_type).to_string(),
            duration_seconds: duration,
            start_time: Some(secs_to_iso(date, rleg.board_time)),
            color: if route.color != 0 {
                Some(color_to_hex(route.color))
            } else {
                None
            },
            text_color: if route.text_color != 0 {
                Some(color_to_hex(route.text_color))
            } else {
                None
            },
            fare: lookup_fare(data, rleg.route_idx, board_stop, alight_stop),
            stops: transit_stops,
            from: from_info,
            to: to_info,
        };

        // if there's a transfer walk between this and the previous transit leg
        if i > 0 {
            let prev = &journey.legs[i - 1];
            let prev_route = &data.routes[prev.route_idx as usize];
            let prev_alight_idx = prev_route.stop_idxs[prev.alight_stop_pos as usize];
            if prev_alight_idx != board_stop_idx {
                // there was a transfer walk
                let prev_stop = &data.stops[prev_alight_idx as usize];
                let transfer_time = rleg.board_time.saturating_sub(prev.alight_time);
                let dist =
                    walker::haversine(prev_stop.lat, prev_stop.lon, board_stop.lat, board_stop.lon)
                        as u32;
                legs.push(Leg::Walk(WalkLeg {
                    walk_type: Some("station_transfer".to_string()),
                    from: Some(Location {
                        location: LatLon {
                            lat: prev_stop.lat as f64,
                            lon: prev_stop.lon as f64,
                        },
                        address: Some(prev_stop.name.clone()),
                        id: Some(prev_stop.id.clone()),
                        stop_id: Some(prev_stop.id.clone()),
                        entrance: None,
                        platform: if prev_stop.platform_code.is_empty() {
                            None
                        } else {
                            Some(prev_stop.platform_code.clone())
                        },
                    }),
                    to: Some(Location {
                        location: LatLon {
                            lat: board_stop.lat as f64,
                            lon: board_stop.lon as f64,
                        },
                        address: Some(board_stop.name.clone()),
                        id: Some(board_stop.id.clone()),
                        stop_id: Some(board_stop.id.clone()),
                        entrance: None,
                        platform: if board_stop.platform_code.is_empty() {
                            None
                        } else {
                            Some(board_stop.platform_code.clone())
                        },
                    }),
                    duration_seconds: transfer_time,
                    distance_meters: Some(dist),
                    polyline: None,
                }));
            }
        }

        legs.push(Leg::Transit(TransitLeg {
            route_options: vec![route_option],
        }));
    }

    // egress walk leg
    if journey.egress_walk_secs > 0 {
        let stop = &data.stops[journey.egress_stop_idx as usize];
        let egress_dist = dest_stops
            .iter()
            .find(|(s, _, _)| *s == journey.egress_stop_idx)
            .map(|(_, _, d)| *d)
            .unwrap_or(0);

        legs.push(Leg::Walk(WalkLeg {
            walk_type: Some("station_access".to_string()),
            from: Some(Location {
                location: LatLon {
                    lat: stop.lat as f64,
                    lon: stop.lon as f64,
                },
                address: Some(stop.name.clone()),
                id: Some(stop.id.clone()),
                stop_id: Some(stop.id.clone()),
                entrance: None,
                platform: if stop.platform_code.is_empty() {
                    None
                } else {
                    Some(stop.platform_code.clone())
                },
            }),
            to: Some(Location {
                location: LatLon {
                    lat: req.dest_lat,
                    lon: req.dest_lon,
                },
                address: Some("END".to_string()),
                id: None,
                stop_id: None,
                entrance: None,
                platform: None,
            }),
            duration_seconds: journey.egress_walk_secs,
            distance_meters: Some(egress_dist),
            polyline: None,
        }));
    }

    let total_duration = journey.arrival_time.saturating_sub(journey.departure_time);
    let start_time = secs_to_iso(&req.date_str, journey.departure_time);

    Some(Plan {
        duration_seconds: total_duration,
        duration_seconds_min: Some(total_duration),
        duration_seconds_max: Some(total_duration),
        start_time,
        legs,
        fares_min: None,
        fares_max: None,
        currency: None,
    })
}

fn lookup_fare(
    data: &TransitData,
    route_idx: u32,
    _from_stop: &crate::data::Stop,
    _to_stop: &crate::data::Stop,
) -> Option<FareInfo> {
    // simple fare lookup: find first matching fare rule for this route
    for fr in &data.fare_rules {
        if fr.route_idx == route_idx || fr.route_idx == NOT_SET {
            return Some(FareInfo {
                base_fare: fr.price,
                final_fare: Some(fr.price),
                currency: "USD".to_string(), // simplified; real impl would store currency
            });
        }
    }
    None
}

// parse walking speed string to m/s. accepts "slow", "normal", "fast" or a numeric m/s value
fn parse_walk_speed(s: Option<&str>) -> f32 {
    match s {
        Some("slow") => 0.8,
        Some("fast") => 1.6,
        Some("normal") => walker::DEFAULT_WALK_SPEED_MPS,
        Some(v) => v.parse::<f32>().unwrap_or(walker::DEFAULT_WALK_SPEED_MPS),
        None => walker::DEFAULT_WALK_SPEED_MPS,
    }
}
