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

    // plan a trip, returns json string matching the wheels router web api
    pub fn plan(&self, request_json: &str) -> Result<String, String> {
        let req: PlanRequestInput =
            serde_json::from_str(request_json).map_err(|e| format!("invalid request: {}", e))?;

        let (origin_lat, origin_lon) = parse_location(&req.origin)?;
        let (dest_lat, dest_lon) = parse_location(&req.destination)?;

        // parse departure time
        let (departure_time, service_day, date_str) = if let Some(ref dt) = req.depart_at {
            parse_datetime(dt)?
        } else {
            // default: now-ish, use 8am as a reasonable default
            (8 * 3600, 0, "2026-01-01".to_string())
        };

        let plan_req = PlanRequest {
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
            walking_speed: req.walking_speed.clone(),
        };

        let response = planner::plan(&self.data, &plan_req);
        serde_json::to_string(&response).map_err(|e| format!("json error: {}", e))
    }

    // get basic stats about loaded data
    pub fn stats(&self) -> String {
        format!(
            "{{\"stops\":{},\"routes\":{},\"trips\":{},\"services\":{}}}",
            self.data.stops.len(),
            self.data.routes.len(),
            self.data.trips.len(),
            self.data.services.len(),
        )
    }
}

// input json format matching the wheels router web api query params
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
        // for stop references, we'd need to look up the stop
        // for now return an error asking for coordinates
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
    // simplified parser for "yyyy-mm-ddThh:mm:ssZ" format
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

    // calculate days since epoch (simplified)
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
    // simplified days since 1970-01-01
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

        // plan a trip, accepts and returns json strings
        pub fn plan(&self, request_json: &str) -> Result<String, JsError> {
            self.inner.plan(request_json).map_err(|e| JsError::new(&e))
        }

        // get stats about loaded data
        pub fn stats(&self) -> String {
            self.inner.stats()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 2025-01-01 should be 20089 days since epoch
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
        // bit 0 = 0, bit 1 = 1, bit 2 = 0, etc
        assert!(!svc.runs_on(100)); // bit 0
        assert!(svc.runs_on(101)); // bit 1
        assert!(!svc.runs_on(102)); // bit 2
        assert!(svc.runs_on(103)); // bit 3
    }
}
