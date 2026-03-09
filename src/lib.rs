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
use types::PlanResponse;

// the main router instance, holds loaded transit data
pub struct Router {
    data: data::TransitData,
}

impl Router {
    // load a .wheelsrouter file from bytes
    pub fn load(bytes: &[u8]) -> Result<Self, String> {
        let data = loader::load(bytes).map_err(|e| format!("failed to load data: {}", e))?;
        Ok(Router { data })
    }

    // plan a trip from a structured request
    pub fn plan(&self, req: &PlanRequest) -> PlanResponse {
        planner::plan(&self.data, req)
    }

    // plan a trip from a json string (convenience wrapper for non-wasm use)
    pub fn plan_json(&self, request_json: &str) -> Result<String, String> {
        let req = parse_request_json(request_json)?;
        let response = planner::plan(&self.data, &req);
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
