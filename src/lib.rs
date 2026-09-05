// wheels router nano
// a wasm-ready transit trip planner using raptor

pub mod data;
pub mod loader;
pub mod planner;
pub mod raptor;
pub mod types;
pub mod walker;

mod transit_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/transit_capnp.rs"));
}

use planner::PlanRequest;
use raptor::RaptorBuffers;
use types::PlanResponse;

// Serde-serializable output types for visualization
#[derive(serde::Serialize)]
pub struct VizStopJson {
    pub stop_idx: u32,
    pub lat: f32,
    pub lon: f32,
    pub name: String,
    pub arrival_secs: u32,
    pub round: u32,
    pub route_color: String, // "#rrggbb"
    pub parent_stop_idx: u32,
    pub route_idx: u32,       // NOT_SET (0xffffffff) if walk/init
    pub board_stop_pos: u32,  // position within route (NOT_SET if walk)
    pub alight_stop_pos: u32, // position within route (NOT_SET if walk)
}

// One route's stop coordinates, for drawing polyline segments in JS
#[derive(serde::Serialize)]
pub struct VizRouteJson {
    pub color: String,          // "#rrggbb"
    pub stops: Vec<(f32, f32)>, // (lat, lon) for every stop in the route
}

#[derive(serde::Serialize)]
pub struct RaptorVizJson {
    pub rounds: Vec<Vec<VizStopJson>>,
    pub departure_time: u32,
    pub num_stops_total: usize,
    // routes that appear in the viz, keyed by route_idx (as string for JSON)
    pub routes: std::collections::HashMap<u32, VizRouteJson>,
}

// the main router instance, holds loaded transit data
pub struct Router {
    data: data::TransitData,
    bufs: std::cell::RefCell<RaptorBuffers>,
}

impl Router {
    // load a .wheelsrouter file from bytes
    pub fn load(bytes: &[u8]) -> Result<Self, String> {
        let data = loader::load(bytes).map_err(|e| format!("failed to load data: {}", e))?;
        let num_stops = data.stops.len();
        let num_routes = data.routes.len();
        let max_transfers = 3; // DEFAULT_MAX_TRANSFERS
        let mut bufs = RaptorBuffers::new(num_stops, max_transfers);
        bufs.ensure_capacity(num_stops, max_transfers, num_routes);
        Ok(Router {
            data,
            bufs: std::cell::RefCell::new(bufs),
        })
    }

    // plan a trip from a structured request
    pub fn plan(&self, req: &PlanRequest) -> PlanResponse {
        planner::plan(&self.data, req, &mut self.bufs.borrow_mut())
    }

    // plan a trip from a json string (convenience wrapper for non-wasm use)
    pub fn plan_json(&self, request_json: &str) -> Result<String, String> {
        let req = parse_request_json(request_json)?;
        let response = planner::plan(&self.data, &req, &mut self.bufs.borrow_mut());
        serde_json::to_string(&response).map_err(|e| format!("json error: {}", e))
    }

    // get basic stats about loaded data
    pub fn stats(&self) -> Stats {
        Stats {
            stops: self.data.stops.len(),
            routes: self.data.routes.len(),
            trips: self.data.trips.len(),
            services: self.data.services.len(),
        }
    }

    // run raptor visualization: returns per-round stop reachability
    pub fn plan_viz(&self, req: &PlanRequest) -> RaptorVizJson {
        let walk_speed = match req.walking_speed.as_deref() {
            Some("slow") => 0.8f32,
            Some("fast") => 1.6f32,
            Some("normal") | None => walker::DEFAULT_WALK_SPEED_MPS,
            Some(v) => v.parse::<f32>().unwrap_or(walker::DEFAULT_WALK_SPEED_MPS),
        };
        let max_walk = req.max_walk_distance.unwrap_or(1200);
        let origin_reach = walker::reachable_stops_from_coord(
            &self.data.walk_graph,
            &self.data.stops,
            req.origin_lat as f32,
            req.origin_lon as f32,
            max_walk,
            walk_speed,
        );
        let origin_stops: Vec<(u32, u32)> = origin_reach
            .stops
            .iter()
            .map(|&(stop_idx, secs, _dist)| (stop_idx, secs))
            .collect();
        let query = raptor::RaptorQuery {
            origin_stops,
            dest_stops: vec![],
            departure_time: req.departure_time,
            service_day: req.service_day,
            max_transfers: req.max_transfers.unwrap_or(3),
            max_results: 1,
        };
        let viz = raptor::run_viz(&self.data, &query);

        // collect all route_idxs that appear in the viz
        let mut route_idxs_used: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for round in &viz.rounds {
            for s in round {
                if s.route_idx != crate::data::NOT_SET {
                    route_idxs_used.insert(s.route_idx);
                }
            }
        }

        // build routes map: route_idx → stop coords + color
        let routes: std::collections::HashMap<u32, VizRouteJson> = route_idxs_used
            .into_iter()
            .map(|ridx| {
                let route = &self.data.routes[ridx as usize];
                let color = if route.color == 0 {
                    "#007aff".to_string()
                } else {
                    format!("#{:06x}", route.color)
                };
                let stops: Vec<(f32, f32)> = route
                    .stop_idxs
                    .iter()
                    .map(|&sidx| {
                        let s = &self.data.stops[sidx as usize];
                        (s.lat, s.lon)
                    })
                    .collect();
                (ridx, VizRouteJson { color, stops })
            })
            .collect();

        let rounds: Vec<Vec<VizStopJson>> = viz
            .rounds
            .into_iter()
            .map(|stops| {
                stops
                    .into_iter()
                    .map(|s| {
                        let color = if s.route_color == 0 {
                            "#007aff".to_string()
                        } else {
                            format!("#{:06x}", s.route_color)
                        };
                        VizStopJson {
                            stop_idx: s.stop_idx,
                            lat: s.lat,
                            lon: s.lon,
                            name: s.name,
                            arrival_secs: s.arrival_secs,
                            round: s.round,
                            route_color: color,
                            parent_stop_idx: s.parent_stop_idx,
                            route_idx: s.route_idx,
                            board_stop_pos: s.board_stop_pos,
                            alight_stop_pos: s.alight_stop_pos,
                        }
                    })
                    .collect()
            })
            .collect();
        RaptorVizJson {
            rounds,
            departure_time: viz.departure_time,
            num_stops_total: self.data.stops.len(),
            routes,
        }
    }

    // street graph for the road assistant: the directed vehicle layer when
    // the build includes one, else the pedestrian layer (older graphs)
    fn assistant_graph(&self) -> &data::WalkGraph {
        if !self.data.road_graph.nodes.is_empty() {
            &self.data.road_graph
        } else {
            &self.data.walk_graph
        }
    }

    // snap a coordinate onto the osm walk/street graph (local road snap)
    pub fn snap_nearest(&self, lat: f64, lon: f64, max_m: f64) -> Option<walker::SnapResult> {
        walker::snap_to_graph(self.assistant_graph(), lat as f32, lon as f32, max_m as f32)
    }

    // route along the osm walk/street graph through waypoints, avoiding
    // blockers. returns the road path, or None when a leg can't be routed.
    pub fn road_route(
        &self,
        waypoints: &[(f64, f64)],
        blockers: &[(f64, f64)],
        avoid_radius_m: f64,
        max_leg_m: u32,
    ) -> Option<Vec<(f64, f64)>> {
        let blocks: Vec<walker::Blocker> = blockers
            .iter()
            .map(|(la, lo)| walker::Blocker {
                lat: *la as f32,
                lon: *lo as f32,
                radius_m: avoid_radius_m as f32,
            })
            .collect();
        let wps: Vec<(f32, f32)> = waypoints
            .iter()
            .map(|(la, lo)| (*la as f32, *lo as f32))
            .collect();
        walker::route_waypoints(self.assistant_graph(), &wps, &blocks, max_leg_m)
            .map(|path| path.into_iter().map(|(la, lo)| (la as f64, lo as f64)).collect())
    }
}

// structured stats response
#[derive(serde::Serialize)]
pub struct Stats {
    pub stops: usize,
    pub routes: usize,
    pub trips: usize,
    pub services: usize,
}

// parse a json request string into a PlanRequest
fn parse_request_json(request_json: &str) -> Result<PlanRequest, String> {
    let req: PlanRequestInput =
        serde_json::from_str(request_json).map_err(|e| format!("invalid request: {}", e))?;
    plan_request_from_input(req)
}

fn plan_request_from_input(req: PlanRequestInput) -> Result<PlanRequest, String> {
    let (origin_lat, origin_lon) = parse_location(&req.origin)?;
    let (dest_lat, dest_lon) = parse_location(&req.destination)?;

    let (departure_time, service_day, date_str) = if let Some(ref dt) = req.depart_at {
        parse_datetime(dt)?
    } else {
        (8 * 3600, 0, "2026-01-01".to_string())
    };

    Ok(PlanRequest {
        origin_lat,
        origin_lon,
        dest_lat,
        dest_lon,
        departure_time,
        service_day,
        date_str,
        max_transfers: req.max_transfers,
        max_results: req.max_results.map(|x| x as usize),
        max_walk_distance: req.max_walk_distance,
        walking_speed: req.walking_speed,
    })
}

// input format for json/js requests
#[derive(serde::Deserialize)]
struct PlanRequestInput {
    origin: String,
    destination: String,
    depart_at: Option<String>,
    #[allow(dead_code)]
    arrive_by: Option<String>,
    #[allow(dead_code)]
    modes: Option<String>,
    max_transfers: Option<u32>,
    max_results: Option<u32>,
    max_walk_distance: Option<u32>,
    walking_speed: Option<String>,
    #[allow(dead_code)]
    modifier: Option<String>,
}

// parse "lat,lon" or "stop:ID" format
fn parse_location(s: &str) -> Result<(f64, f64), String> {
    if s.starts_with("stop:") {
        return Err("stop: references not yet supported, use lat,lon".to_string());
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 2 {
        return Err(format!("invalid location format: {}", s));
    }
    let lat: f64 = parts[0]
        .trim()
        .parse()
        .map_err(|_| format!("invalid latitude: {}", parts[0]))?;
    let lon: f64 = parts[1]
        .trim()
        .parse()
        .map_err(|_| format!("invalid longitude: {}", parts[1]))?;
    Ok((lat, lon))
}

// parse iso 8601 datetime to (seconds_since_midnight, epoch_day, date_string)
fn parse_datetime(s: &str) -> Result<(u32, u16, String), String> {
    let s = s.trim().trim_end_matches('Z');
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return Err(format!("invalid datetime: {}", s));
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    if date_parts.len() != 3 {
        return Err(format!("invalid date: {}", parts[0]));
    }
    let year: i32 = date_parts[0].parse().map_err(|_| "bad year")?;
    let month: u32 = date_parts[1].parse().map_err(|_| "bad month")?;
    let day: u32 = date_parts[2].parse().map_err(|_| "bad day")?;

    let epoch_day = days_since_epoch(year, month, day);

    let time_parts: Vec<&str> = parts[1].split(':').collect();
    if time_parts.len() < 2 {
        return Err(format!("invalid time: {}", parts[1]));
    }
    let h: u32 = time_parts[0].parse().map_err(|_| "bad hour")?;
    let m: u32 = time_parts[1].parse().map_err(|_| "bad minute")?;
    let sec: u32 = if time_parts.len() > 2 {
        time_parts[2].parse().unwrap_or(0)
    } else {
        0
    };

    Ok((
        h * 3600 + m * 60 + sec,
        epoch_day as u16,
        parts[0].to_string(),
    ))
}

fn days_since_epoch(year: i32, month: u32, day: u32) -> i32 {
    let mut y = year;
    let mut m = month as i32;
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// wasm bindings
#[cfg(feature = "wasm")]
mod wasm {
    use super::*;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    pub struct WasmRouter {
        inner: Router,
    }

    #[wasm_bindgen]
    impl WasmRouter {
        // load .wheelsrouter data from a uint8array
        #[wasm_bindgen(constructor)]
        pub fn new(data: &[u8]) -> Result<WasmRouter, JsError> {
            let router = Router::load(data).map_err(|e| JsError::new(&e))?;
            Ok(WasmRouter { inner: router })
        }

        // plan a trip. accepts a js object, returns a js object - no json serialization needed.
        pub fn plan(&self, request: JsValue) -> Result<JsValue, JsError> {
            let input: PlanRequestInput = serde_wasm_bindgen::from_value(request)
                .map_err(|e| JsError::new(&format!("invalid request: {}", e)))?;
            let req = plan_request_from_input(input).map_err(|e| JsError::new(&e))?;
            let response = self.inner.plan(&req);
            serde_wasm_bindgen::to_value(&response)
                .map_err(|e| JsError::new(&format!("serialization error: {}", e)))
        }

        // get stats about loaded data as a js object
        pub fn stats(&self) -> Result<JsValue, JsError> {
            let s = self.inner.stats();
            serde_wasm_bindgen::to_value(&s)
                .map_err(|e| JsError::new(&format!("serialization error: {}", e)))
        }

        // run raptor visualization: returns per-round stop reachability as a js object
        pub fn plan_viz(&self, request: JsValue) -> Result<JsValue, JsError> {
            let input: PlanRequestInput = serde_wasm_bindgen::from_value(request)
                .map_err(|e| JsError::new(&format!("invalid request: {}", e)))?;
            let req = plan_request_from_input(input).map_err(|e| JsError::new(&e))?;
            let viz = self.inner.plan_viz(&req);
            serde_wasm_bindgen::to_value(&viz)
                .map_err(|e| JsError::new(&format!("serialization error: {}", e)))
        }

        // snap a coordinate onto the osm walk/street graph (fully local).
        // request: { lat, lon, maxM? } → { ok, lat, lon, distM }
        pub fn snap(&self, request: JsValue) -> Result<JsValue, JsError> {
            #[derive(serde::Deserialize)]
            struct SnapInput {
                lat: f64,
                lon: f64,
                #[serde(rename = "maxM", default)]
                max_m: Option<f64>,
            }
            #[derive(serde::Serialize)]
            #[allow(non_snake_case)]
            struct SnapOut {
                ok: bool,
                lat: f64,
                lon: f64,
                distM: f64,
            }
            let input: SnapInput = serde_wasm_bindgen::from_value(request)
                .map_err(|e| JsError::new(&format!("invalid request: {}", e)))?;
            let out = match self
                .inner
                .snap_nearest(input.lat, input.lon, input.max_m.unwrap_or(150.0))
            {
                Some(s) => SnapOut {
                    ok: true,
                    lat: s.lat as f64,
                    lon: s.lon as f64,
                    distM: s.dist_m as f64,
                },
                None => SnapOut {
                    ok: false,
                    lat: input.lat,
                    lon: input.lon,
                    distM: f64::INFINITY,
                },
            };
            serde_wasm_bindgen::to_value(&out)
                .map_err(|e| JsError::new(&format!("serialization error: {}", e)))
        }

        // route along the osm walk/street graph through waypoints, avoiding
        // blockers (fully local).
        // request: { waypoints: [[lat,lon]…], avoid?: [[lat,lon]…],
        //            avoidRadiusM?, maxLegM? } → { ok, path, meters } | { ok: false }
        pub fn road_route(&self, request: JsValue) -> Result<JsValue, JsError> {
            #[derive(serde::Deserialize)]
            struct RouteInput {
                waypoints: Vec<(f64, f64)>,
                #[serde(default)]
                avoid: Vec<(f64, f64)>,
                #[serde(rename = "avoidRadiusM", default)]
                avoid_radius_m: Option<f64>,
                #[serde(rename = "maxLegM", default)]
                max_leg_m: Option<u32>,
            }
            let input: RouteInput = serde_wasm_bindgen::from_value(request)
                .map_err(|e| JsError::new(&format!("invalid request: {}", e)))?;
            match self.inner.road_route(
                &input.waypoints,
                &input.avoid,
                input.avoid_radius_m.unwrap_or(40.0),
                input.max_leg_m.unwrap_or(4000),
            ) {
                Some(path) => {
                    let meters: f64 = path
                        .windows(2)
                        .map(|w| {
                            walker::haversine(
                                w[0].0 as f32,
                                w[0].1 as f32,
                                w[1].0 as f32,
                                w[1].1 as f32,
                            ) as f64
                        })
                        .sum();
                    #[derive(serde::Serialize)]
                    struct RouteOut {
                        ok: bool,
                        path: Vec<(f64, f64)>,
                        meters: f64,
                    }
                    serde_wasm_bindgen::to_value(&RouteOut {
                        ok: true,
                        path,
                        meters,
                    })
                }
                None => serde_wasm_bindgen::to_value(&serde_json::json!({ "ok": false })),
            }
            .map_err(|e| JsError::new(&format!("serialization error: {}", e)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::*;
    use crate::raptor::{run as raptor_run, RaptorQuery};

    // build a minimal TransitData with one frequency-based route:
    //   stop 0 -> stop 1 -> stop 2
    //   template: dep at 0=06:00, arr/dep 1=06:10, arr 2=06:20
    //   frequency rule: start=08:00 end=10:00 headway=600s (10 min)
    //   service_idx=0, service runs on epoch_day=20000
    fn make_freq_data() -> TransitData {
        // stops
        let stops = vec![
            Stop {
                id: "S0".into(),
                name: "Stop0".into(),
                lat: 0.0,
                lon: 0.0,
                location_type: 0,
                parent_idx: NOT_SET,
                platform_code: "".into(),
                zone_id: "".into(),
                transfers_offset: 0,
                num_transfers: 0,
                route_idxs: vec![0],
                walk_node_idx: NOT_SET,
            },
            Stop {
                id: "S1".into(),
                name: "Stop1".into(),
                lat: 0.001,
                lon: 0.0,
                location_type: 0,
                parent_idx: NOT_SET,
                platform_code: "".into(),
                zone_id: "".into(),
                transfers_offset: 0,
                num_transfers: 0,
                route_idxs: vec![0],
                walk_node_idx: NOT_SET,
            },
            Stop {
                id: "S2".into(),
                name: "Stop2".into(),
                lat: 0.002,
                lon: 0.0,
                location_type: 0,
                parent_idx: NOT_SET,
                platform_code: "".into(),
                zone_id: "".into(),
                transfers_offset: 0,
                num_transfers: 0,
                route_idxs: vec![0],
                walk_node_idx: NOT_SET,
            },
        ];

        // one trip: template departs stop0 at 06:00 (21600s), stop1 at 06:10 (21600+600), stop2 at 06:20 (21600+1200)
        let template_first_dep: u32 = 6 * 3600; // 21600
        let trips = vec![Trip {
            id: "T0".into(),
            route_idx: 0,
            service_idx: 0,
            headsign: "".into(),
            direction_id: 0,
            frequency_rules: vec![FrequencyRule {
                start_time: 8 * 3600, // 28800
                end_time: 10 * 3600,  // 36000
                headway_secs: 600,
                exact_times: false,
            }],
            template_first_departure: template_first_dep,
        }];

        // one route with 1 trip, 3 stops
        // arrivals/departures layout: 1 trip * 3 stops = 3 entries
        // stop0: arr=21600 dep=21600, stop1: arr=22200 dep=22200, stop2: arr=22800 dep=22800
        let arrivals = vec![21600u32, 22200, 22800];
        let departures = vec![21600u32, 22200, 22800];

        let routes = vec![Route {
            id: "R0".into(),
            short_name: "1".into(),
            long_name: "".into(),
            route_type: 3,
            color: 0,
            text_color: 0,
            agency_idx: 0,
            stop_idxs: vec![0, 1, 2],
            stop_times_offset: 0,
            num_trips: 1,
            trip_idxs: vec![0],
        }];

        // service: always active on epoch_day 20000
        // bit offset = 20000 - start_date; set start_date=20000 so offset=0, bit0=1
        let services = vec![Service {
            id: "SVC".into(),
            start_date: 20000,
            day_bits: vec![0b00000001],
        }];

        TransitData {
            feed_id: "test".into(),
            timezone: "UTC".into(),
            stops,
            routes,
            trips,
            agencies: vec![Agency {
                id: "A".into(),
                name: "Agency".into(),
                url: "".into(),
            }],
            arrivals,
            departures,
            transfers: vec![],
            services,
            walk_graph: WalkGraph::default(),
            road_graph: WalkGraph::default(),
            fare_rules: vec![],
        }
    }

    // test that raptor correctly finds a frequency trip.
    // query: depart stop0 at 08:05 (28800+300), service_day=20000
    // expected: board at stop0 at 08:10 (next headway after 08:05), alight stop2 at 08:30
    #[test]
    fn test_frequency_trip_raptor() {
        let data = make_freq_data();
        // depart at 08:05 = 28800+300 = 29100
        let query = RaptorQuery {
            origin_stops: vec![(0, 0)], // stop 0, 0 walk seconds
            dest_stops: vec![(2, 0)],   // stop 2, 0 walk seconds
            departure_time: 29100,      // 08:05
            service_day: 20000,
            max_transfers: 3,
            max_results: 5,
        };
        let journeys = raptor_run(&data, &query);
        assert!(
            !journeys.is_empty(),
            "expected at least one journey for frequency trip"
        );
        let j = &journeys[0];
        assert_eq!(j.legs.len(), 1, "expected 1 leg");
        let leg = &j.legs[0];
        // next trip after 08:05: start=08:00, first dep at stop0=08:00, headway=600
        // steps = ceil((29100-28800)/600) = ceil(0.5) = 1 => next dep = 28800 + 600 = 29400 (08:10)
        assert_eq!(
            leg.board_time, 29400,
            "board time should be 08:10 = 29400, got {}",
            leg.board_time
        );
        // stop2 arr = stop0 dep + 1200s = 29400 + 1200 = 30600 (08:30)
        assert_eq!(
            leg.alight_time, 30600,
            "alight time should be 08:30 = 30600, got {}",
            leg.alight_time
        );
    }

    #[test]
    fn test_parse_location() {
        let (lat, lon) = parse_location("22.297432,114.237296").unwrap();
        assert!((lat - 22.297432).abs() < 0.0001);
        assert!((lon - 114.237296).abs() < 0.0001);
    }

    #[test]
    fn test_parse_datetime() {
        let (secs, _day, date) = parse_datetime("2025-11-25T09:13:00Z").unwrap();
        assert_eq!(secs, 9 * 3600 + 13 * 60);
        assert_eq!(date, "2025-11-25");
    }

    #[test]
    fn test_days_since_epoch() {
        let d = days_since_epoch(2025, 1, 1);
        assert_eq!(d, 20089);
    }

    #[test]
    fn test_service_bitfield() {
        use crate::data::Service;
        let svc = Service {
            id: "test".to_string(),
            start_date: 100,
            day_bits: vec![0b10101010, 0b01010101],
        };
        assert!(!svc.runs_on(100));
        assert!(svc.runs_on(101));
        assert!(!svc.runs_on(102));
        assert!(svc.runs_on(103));
    }

    #[test]
    fn test_hk_data_diagnostic() {
        let path = "data/hk.wheelsrouter";
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                eprintln!("skipping: hk data not found");
                return;
            }
        };
        let data = crate::loader::load(&bytes).expect("failed to load hk data");

        // basic stats
        println!(
            "hk: {} stops, {} routes, {} trips, {} services",
            data.stops.len(),
            data.routes.len(),
            data.trips.len(),
            data.services.len()
        );

        // count frequency vs schedule trips
        let freq_trips = data
            .trips
            .iter()
            .filter(|t| !t.frequency_rules.is_empty())
            .count();
        let sched_trips = data
            .trips
            .iter()
            .filter(|t| t.frequency_rules.is_empty())
            .count();
        println!(
            "hk: {} frequency trips, {} schedule trips",
            freq_trips, sched_trips
        );

        // examine service calendars: what epoch days do they cover?
        // 2026-03-10 = ?
        let today_epoch = days_since_epoch(2026, 3, 10) as u16;
        println!("today epoch day (2026-03-10): {}", today_epoch);
        let mut active_services = 0;
        for (i, svc) in data.services.iter().enumerate() {
            let max_day = svc.start_date as usize + svc.day_bits.len() * 8;
            let runs_today = svc.runs_on(today_epoch);
            if runs_today {
                active_services += 1;
            }
            if i < 10 || runs_today {
                println!(
                    "  svc[{}] id={} start_date={} max_day={} runs_today={}",
                    i, svc.id, svc.start_date, max_day, runs_today
                );
            }
        }
        println!(
            "services active on 2026-03-10: {}/{}",
            active_services,
            data.services.len()
        );

        // count trips with active service today
        let mut trips_today = 0;
        let mut freq_trips_today = 0;
        for trip in &data.trips {
            if trip.service_idx != NOT_SET
                && data.services[trip.service_idx as usize].runs_on(today_epoch)
            {
                trips_today += 1;
                if !trip.frequency_rules.is_empty() {
                    freq_trips_today += 1;
                }
            }
        }
        println!(
            "trips active today: {} ({} frequency)",
            trips_today, freq_trips_today
        );

        // examine a few frequency trips
        let mut shown = 0;
        for (i, trip) in data.trips.iter().enumerate() {
            if !trip.frequency_rules.is_empty() && shown < 5 {
                let route = &data.routes[trip.route_idx as usize];
                println!(
                    "  freq trip[{}] id={} route={} template_first_dep={} rules={}",
                    i,
                    trip.id,
                    route.short_name,
                    trip.template_first_departure,
                    trip.frequency_rules.len()
                );
                for (j, rule) in trip.frequency_rules.iter().enumerate() {
                    println!(
                        "    rule[{}]: start={} end={} headway={}s exact={}",
                        j, rule.start_time, rule.end_time, rule.headway_secs, rule.exact_times
                    );
                }
                // show stop times for this trip
                let route_idx = trip.route_idx as usize;
                let r = &data.routes[route_idx];
                // find which trip_num this is within the route
                let trip_num = r.trip_idxs.iter().position(|&ti| ti == i as u32);
                if let Some(tn) = trip_num {
                    println!("    trip_num in route: {}", tn);
                    for pos in 0..r.stop_idxs.len().min(5) {
                        let arr = r.arrival_at(&data.arrivals, tn as u32, pos);
                        let dep = r.departure_at(&data.departures, tn as u32, pos);
                        let stop = &data.stops[r.stop_idxs[pos] as usize];
                        println!("      stop[{}] {} arr={} dep={}", pos, stop.name, arr, dep);
                    }
                }
                shown += 1;
            }
        }

        // try a raptor query: Tsim Sha Tsui area -> Central area
        // use the first active service day
        let query_dep = 9 * 3600; // 09:00
        let origin_stops: Vec<(u32, u32)> = data
            .stops
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let dlat = (s.lat - 22.2988).abs();
                let dlon = (s.lon - 114.1722).abs();
                dlat < 0.005 && dlon < 0.005
            })
            .map(|(i, _)| (i as u32, 60)) // 60s walk
            .take(20)
            .collect();
        let dest_stops: Vec<(u32, u32)> = data
            .stops
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let dlat = (s.lat - 22.2819).abs();
                let dlon = (s.lon - 114.1589).abs();
                dlat < 0.005 && dlon < 0.005
            })
            .map(|(i, _)| (i as u32, 60))
            .take(20)
            .collect();
        println!("origin stops (TST area): {}", origin_stops.len());
        println!("dest stops (Central area): {}", dest_stops.len());

        if !origin_stops.is_empty() && !dest_stops.is_empty() {
            let query = crate::raptor::RaptorQuery {
                origin_stops,
                dest_stops,
                departure_time: query_dep,
                service_day: today_epoch,
                max_transfers: 3,
                max_results: 5,
            };
            let journeys = crate::raptor::run(&data, &query);
            println!("raptor journeys found: {}", journeys.len());
            for (i, j) in journeys.iter().enumerate() {
                println!(
                    "  journey[{}]: dep={} arr={} legs={}",
                    i,
                    j.departure_time,
                    j.arrival_time,
                    j.legs.len()
                );
                for leg in &j.legs {
                    let route = &data.routes[leg.route_idx as usize];
                    let board_stop =
                        &data.stops[route.stop_idxs[leg.board_stop_pos as usize] as usize];
                    let alight_stop =
                        &data.stops[route.stop_idxs[leg.alight_stop_pos as usize] as usize];
                    println!(
                        "    {} ({}): {} -> {} board={} alight={} freq_delta={}",
                        route.short_name,
                        route.id,
                        board_stop.name,
                        alight_stop.name,
                        leg.board_time,
                        leg.alight_time,
                        leg.freq_delta
                    );
                }
            }
        }

        assert!(
            active_services > 0,
            "no services active on 2026-03-10 — calendar problem!"
        );
    }
}
